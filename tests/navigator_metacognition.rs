//! The metacognition's tool loop, over a scripted wiremock chat provider:
//! one `read` round then a remark; the tool-call cap forcing a final
//! tools-free call; the whole-remark 8 s budget returning silence; the
//! open-buffer override answering from the editor text instead of disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rust_agent::navigator::commenter::{Commenter, LlmCommenter, RemarkContext};
use rust_agent::navigator::log::NavLog;
use rust_agent::provider::config::{
    ApiKind, ModelConfig, ProviderConfig, ReasoningHistory, ResolvedModel,
};
use rust_agent::provider::openai::ClientCache;

fn resolved(base_url: String) -> ResolvedModel {
    ResolvedModel {
        alias: "metacognition".to_string(),
        provider_name: "mock".to_string(),
        provider: ProviderConfig {
            base_url,
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 600_000,
        },
        model: ModelConfig {
            provider: "mock".to_string(),
            model: "mock-fast".to_string(),
            api: ApiKind::Chat,
            context_window: None,
            max_output_tokens: None,
            supports_tools: true,
            supports_parallel_tool_calls: None,
            thinking: None,
            reasoning_history: ReasoningHistory::Preserve,
        },
    }
}

fn commenter(base_url: String, log_path: Option<PathBuf>) -> LlmCommenter {
    let mut c = LlmCommenter::new(
        resolved(base_url),
        "test-key".into(),
        Arc::new(ClientCache::new()),
    );
    if let Some(p) = log_path {
        c = c.with_log(NavLog::new(p));
    }
    c
}

fn remark_ctx(ws: &Path, buffers: Vec<(PathBuf, &str)>) -> RemarkContext {
    let open_buffers: BTreeMap<PathBuf, String> = buffers
        .into_iter()
        .map(|(p, t)| (p, t.to_string()))
        .collect();
    RemarkContext::build(
        "file:///ws/src/main.rs",
        "rust",
        "a\nb\n",
        "a\nB\n",
        Vec::new(),
        None,
    )
    .into_context(ws.to_path_buf(), open_buffers)
}

/// One SSE data line carrying `payload` (already-serialized JSON value).
fn sse(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

fn sse_stream(lines: Vec<String>) -> String {
    let mut out = lines.concat();
    out.push_str("data: [DONE]\n\n");
    out
}

/// A text-only assistant delta.
fn delta_text(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
}

/// A tool-call assistant delta (single chunk, full arguments).
fn delta_tool_call(id: &str, name: &str, args: &Value) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,
        "function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
}

async fn received_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("wiremock retains requests")
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("chat request body"))
        .collect()
}

// Given a wiremock chat provider where the first round asks to `read` a
// file and the second answers the remark
// When the metacognition remarks with a workspace
// Then the remark is the model's JSON, the read hit disk, and the log
//      recorded the round and the tool execution.
#[tokio::test]
async fn one_read_round_then_a_remark() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/other.rs"), "fn helper() {}\n").unwrap();

    let server = MockServer::start().await;
    // Round 1: tool call. Round 2: the remark.
    let round1 = sse_stream(vec![sse(&delta_tool_call(
        "call-1",
        "read",
        &json!({"path": "src/other.rs"}),
    ))]);
    let answer = r#"{"line": 2, "text": "helper is fine"}"#;
    let round2 = sse_stream(vec![sse(&delta_text(answer))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"tools": [{"type": "function"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(round1, "text/event-stream"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(round2, "text/event-stream"))
        .mount(&server)
        .await;

    let log_path = ws.join("nav.jsonl");
    let c = commenter(server.uri(), Some(log_path.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = remark_ctx(&ws, vec![]);
    let remark = c.remark(&ctx, &cancel).await.unwrap().expect("a remark");
    assert_eq!(remark.line, 1);
    assert_eq!(remark.text, "helper is fine");

    // The second request fed the tool result back as a Tool message.
    let bodies = received_bodies(&server).await;
    assert_eq!(bodies.len(), 2, "one tool round then one answer");
    let roles: Vec<&str> = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);

    // The log: a round, a tool, then the final round.
    let log = NavLog::new(&log_path);
    let records = log.read_all();
    let layers: Vec<&str> = records
        .iter()
        .filter(|r| r["kind"] == "call")
        .map(|r| r["layer"].as_str().unwrap())
        .collect();
    assert_eq!(
        layers,
        vec![
            "metacognition_round",
            "metacognition_tool",
            "metacognition_round"
        ]
    );
    let tool_records: Vec<&Value> = records
        .iter()
        .filter(|r| r["layer"] == "metacognition_tool")
        .collect();
    assert_eq!(tool_records[0]["model"], "read");
}

// Given a model that keeps calling tools
// When the metacognition remarks
// Then after 2 tool rounds the third call carries NO tools and its answer
//      is the remark.
#[tokio::test]
async fn tool_loop_is_capped_at_two_rounds_then_forces_the_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::write(ws.join("a.rs"), "a\n").unwrap();

    let server = MockServer::start().await;
    let tool_round = sse_stream(vec![sse(&delta_tool_call(
        "c",
        "read",
        &json!({"path": "a.rs"}),
    ))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"tools": [{"type": "function"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(tool_round, "text/event-stream"))
        .mount(&server)
        .await;
    let final_answer = sse_stream(vec![sse(&delta_text(r#"{"silent": true}"#))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(final_answer, "text/event-stream"))
        .mount(&server)
        .await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = remark_ctx(&ws, vec![]);
    let out = c.remark(&ctx, &cancel).await.unwrap();
    assert!(out.is_none(), "the forced final answer is silence");

    let bodies = received_bodies(&server).await;
    assert_eq!(bodies.len(), 3, "two tool rounds then the forced call");
    let last = bodies.last().unwrap();
    assert!(
        last.get("tools").is_none(),
        "the final call carries no tools: {last}"
    );
    // The two first rounds offered exactly read and grep.
    for b in &bodies[..2] {
        let names: Vec<&str> = b["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read", "grep"]);
    }
}

// Given a provider whose first response never arrives in time
// When the whole remark exceeds its 8 s budget
// Then the metacognition returns silence (never an error) and the log
//      records the cancelled round.
#[tokio::test]
async fn over_budget_returns_silence() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();

    let server = MockServer::start().await;
    let slow = sse_stream(vec![sse(&delta_text(r#"{"line": 1, "text": "late"}"#))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(20))
                .set_body_raw(slow, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let log_path = ws.join("nav.jsonl");
    let c = commenter(server.uri(), Some(log_path.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = remark_ctx(&ws, vec![]);
    // Paused clock: the mock's 20 s delay and the 8 s budget both advance
    // in virtual time, so the test itself is instant.
    tokio::time::pause();
    let started = std::time::Instant::now();
    let out = c.remark(&ctx, &cancel).await.unwrap();
    assert!(out.is_none(), "over budget means silence");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the paused clock keeps the test fast"
    );

    let log = NavLog::new(&log_path);
    let records = log.read_all();
    let cancelled_rounds: Vec<&Value> = records
        .iter()
        .filter(|r| r["kind"] == "call" && r["layer"] == "metacognition_round")
        .collect();
    assert!(
        !cancelled_rounds.is_empty() && cancelled_rounds.iter().any(|r| r["cancelled"] == true),
        "the timed-out round is logged as cancelled"
    );
}

// Given a path present in the open buffers whose disk content differs
// When the model calls `read` on it
// Then the tool result is the buffer's text (the editor's truth), not the
//      file on disk.
#[tokio::test]
async fn read_of_an_open_buffer_answers_from_the_buffer() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    let file = ws.join("src/main.rs");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(&file, "OLD DISK TEXT\n").unwrap();
    let buffer_text = "EDITED IN THE BUFFER\nsecond line\n";

    let server = MockServer::start().await;
    let round1 = sse_stream(vec![sse(&delta_tool_call(
        "call-1",
        "read",
        &json!({"path": file.to_string_lossy()}),
    ))]);
    let round2 = sse_stream(vec![sse(&delta_text(r#"{"silent": true}"#))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"tools": [{"type": "function"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(round1, "text/event-stream"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(round2, "text/event-stream"))
        .mount(&server)
        .await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = remark_ctx(&ws, vec![(file.clone(), buffer_text)]);
    let out = c.remark(&ctx, &cancel).await.unwrap();
    assert!(out.is_none());

    let bodies = received_bodies(&server).await;
    let tool_msg = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool result message");
    assert_eq!(
        tool_msg["content"].as_str().unwrap(),
        buffer_text,
        "the buffer text, not the disk file"
    );
}

// Offset/limit on an open buffer slice exactly like ReadTool slices a file.
#[tokio::test]
async fn buffer_read_honours_offset_and_limit() {
    use rust_agent::navigator::commenter::slice_buffer as slice;
    // Same semantics as ReadTool's slice_lines: `lines()` + join, so the
    // trailing newline is not preserved.
    assert_eq!(slice("l1\nl2\nl3\n", None, None), "l1\nl2\nl3\n");
    assert_eq!(slice("l1\nl2\nl3\n", Some(2), None), "l2\nl3");
    assert_eq!(slice("l1\nl2\nl3\n", Some(2), Some(1)), "l2");
    assert_eq!(slice("l1\nl2\nl3\n", Some(9), None), "");
}
