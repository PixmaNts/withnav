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

use withnav::navigator::log::NavLog;
use withnav::navigator::metacognition::{LlmCommenter, RemarkContext};
use withnav::navigator::ports::Metacognition;
use withnav::provider::config::{
    ApiKind, ModelConfig, ProviderConfig, ReasoningHistory, ResolvedModel,
};
use withnav::provider::openai::ClientCache;

mod common;
use common::navigator::{delta_text, delta_tool_call, sse_data, sse_stream};

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
            catalog_format: None,
        },
        model: ModelConfig {
            provider: "mock".to_string(),
            model: "mock-fast".to_string(),
            api: ApiKind::Chat,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            supports_tools: true,
            supports_parallel_tool_calls: None,
            thinking: None,
            default_variant: None,
            variants: None,
            reasoning_history: ReasoningHistory::Preserve,
        },
    }
}

fn commenter(base_url: String, log_path: Option<PathBuf>) -> LlmCommenter {
    commenter_with_limit(base_url, log_path, None)
}

fn commenter_with_limit(
    base_url: String,
    log_path: Option<PathBuf>,
    max_output_tokens: Option<u64>,
) -> LlmCommenter {
    let mut model = resolved(base_url);
    model.model.max_output_tokens = max_output_tokens;
    let mut c = LlmCommenter::new(model, "test-key".into(), Arc::new(ClientCache::new()));
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

async fn received_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("wiremock retains requests")
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("chat request body"))
        .collect()
}

async fn isolated_server() -> MockServer {
    // A cancelled remark can outlive its client future in the HTTP server.
    // Do not recycle that server's address/state into another parallel test.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock listener");
    MockServer::builder().listener(listener).start().await
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

    let server = isolated_server().await;
    // Round 1: tool call. Round 2: the remark.
    let round1 = sse_stream(vec![sse_data(&delta_tool_call(
        "call-1",
        "read",
        &json!({"path": "src/other.rs"}),
    ))]);
    let answer = r#"{"line": 2, "text": "helper is fine"}"#;
    let round2 = sse_stream(vec![sse_data(&delta_text(answer))]);
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
    let mut ctx = remark_ctx(&ws, vec![]);
    ctx.tool_rounds = 2;
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
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool"],
        "server={}; requests={bodies:#?}",
        server.uri()
    );

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
// Then after the 2 tool rounds the context allows, the third call carries NO
//      tools and its answer is the remark.
#[tokio::test]
async fn tool_loop_is_capped_then_forces_the_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::write(ws.join("a.rs"), "a\n").unwrap();

    let server = isolated_server().await;
    let tool_round = sse_stream(vec![sse_data(&delta_tool_call(
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
    let final_answer = sse_stream(vec![sse_data(&delta_text(r#"{"silent": true}"#))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(final_answer, "text/event-stream"))
        .mount(&server)
        .await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = remark_ctx(&ws, vec![]);
    ctx.tool_rounds = 2;
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
// When the whole remark exceeds its 20 s budget
// Then the metacognition returns silence (never an error) and the log
//      records the cancelled round.
#[tokio::test]
async fn over_budget_returns_silence() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();

    let server = isolated_server().await;
    let slow = sse_stream(vec![sse_data(&delta_text(
        r#"{"line": 1, "text": "late"}"#,
    ))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(60))
                .set_body_raw(slow, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let log_path = ws.join("nav.jsonl");
    let c = commenter(server.uri(), Some(log_path.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = remark_ctx(&ws, vec![]);
    // Paused clock: the mock's 60 s delay and the 20 s budget both advance
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

    let server = isolated_server().await;
    let round1 = sse_stream(vec![sse_data(&delta_tool_call(
        "call-1",
        "read",
        &json!({"path": file.to_string_lossy()}),
    ))]);
    let round2 = sse_stream(vec![sse_data(&delta_text(r#"{"silent": true}"#))]);
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
    use withnav::navigator::metacognition::slice_buffer as slice;
    // Same semantics as ReadTool's slice_lines: `lines()` + join, so the
    // trailing newline is not preserved.
    assert_eq!(slice("l1\nl2\nl3\n", None, None), "l1\nl2\nl3\n");
    assert_eq!(slice("l1\nl2\nl3\n", Some(2), None), "l2\nl3");
    assert_eq!(slice("l1\nl2\nl3\n", Some(2), Some(1)), "l2");
    assert_eq!(slice("l1\nl2\nl3\n", Some(9), None), "");
}

// Given a context carrying the rendered pipeline branch prompt
// When the metacognition remarks through `Metacognition::remark`
// Then the system message the model receives contains that prompt.
#[tokio::test]
async fn the_branch_prompt_reaches_the_models_system_message() {
    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    let answer = sse_stream(vec![sse_data(&delta_text(r#"{"silent": true}"#))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
        .mount(&server)
        .await;

    let c = commenter(server.uri(), None);
    let mut ctx = remark_ctx(tmp.path(), vec![]);
    ctx.prompt = Some("Conversation:\ndriver: keep the parser".to_string());
    let cancel = tokio_util::sync::CancellationToken::new();
    let _ = c.remark(&ctx, &cancel).await.unwrap();

    let bodies = received_bodies(&server).await;
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert!(
        system.contains("driver: keep the parser"),
        "branch prompt in the system message: {system}; server={}; requests={bodies:#?}",
        server.uri()
    );
}

// Given a model answering prose instead of the JSON contract
// When the metacognition remarks
// Then the error is the typed `Malformed` carrying the raw output, not a
//      provider failure.
#[tokio::test]
async fn a_prose_answer_is_a_typed_malformed_error() {
    use withnav::navigator::ports::ModelError;

    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    let answer = sse_stream(vec![sse_data(&delta_text("you should refactor this"))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
        .mount(&server)
        .await;

    let c = commenter(server.uri(), None);
    let ctx = remark_ctx(tmp.path(), vec![]);
    let cancel = tokio_util::sync::CancellationToken::new();
    match c.remark(&ctx, &cancel).await {
        Err(ModelError::Malformed(raw)) => assert_eq!(raw, "you should refactor this"),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

// Given a model whose config states no output limit, a larger one, or a
//       smaller one
// When the metacognition remarks
// Then the request never asks for more than 1024 tokens, and keeps a
//      smaller configured limit.
#[tokio::test]
async fn the_remark_request_is_capped_at_1024_tokens() {
    for (configured, expected) in [(None, 1024), (Some(8192), 1024), (Some(256), 256)] {
        let tmp = tempfile::tempdir().unwrap();
        let server = isolated_server().await;
        let answer = sse_stream(vec![sse_data(&delta_text(r#"{"silent": true}"#))]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
            .mount(&server)
            .await;

        let c = commenter_with_limit(server.uri(), None, configured);
        let ctx = remark_ctx(tmp.path(), vec![]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let _ = c.remark(&ctx, &cancel).await.unwrap();

        let bodies = received_bodies(&server).await;
        assert_eq!(
            bodies[0]["max_tokens"], expected,
            "configured limit {configured:?}"
        );
    }
}

/// Mounts one scripted chat answer per request, in order.
async fn script_texts(server: &MockServer, texts: &[&str]) {
    for text in texts {
        let body = sse_stream(vec![sse_data(&delta_text(text))]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

fn user_messages(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect()
}

const DSML_CALL: &str =
    "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read\">path=src/a.rs</｜｜DSML｜｜ invoke>";

// Given a model that keeps calling tools
// When the final round is reached
// Then that request, and only that one, ends with the instruction to answer
//      now in the JSON form.
#[tokio::test]
async fn the_final_round_tells_the_model_to_answer_now() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::write(ws.join("a.rs"), "a\n").unwrap();
    let server = isolated_server().await;
    let tool_round = sse_stream(vec![sse_data(&delta_tool_call(
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
    script_texts(&server, &[r#"{"silent": true}"#]).await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = remark_ctx(&ws, vec![]);
    ctx.tool_rounds = 2;
    c.remark(&ctx, &cancel).await.unwrap();

    let bodies = received_bodies(&server).await;
    assert_eq!(bodies.len(), 3);
    for body in &bodies[..2] {
        assert!(
            !user_messages(body)
                .iter()
                .any(|m| m.contains("No more tools")),
            "tools were still offered: {body}"
        );
    }
    let last = bodies[2]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "user");
    let notice = last["content"].as_str().unwrap();
    assert!(notice.contains("No more tools are available"), "{notice}");
    assert!(notice.contains("JSON only"), "{notice}");
    assert!(notice.contains("\"silent\": true"), "{notice}");
}

// Given a model that writes tool-call markup as its final answer
// When the metacognition remarks
// Then it is asked once more, and the valid second answer is the remark.
#[tokio::test]
async fn a_tool_attempt_in_the_final_answer_is_asked_again_once() {
    for leaked in [
        DSML_CALL,
        "<tool_call>{\"name\": \"read\"}</tool_call>",
        "<function_calls><invoke name=\"read\"/></function_calls>",
        "{\"tool_calls\": [{\"name\": \"read\"}]}",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let server = isolated_server().await;
        script_texts(&server, &[leaked, r#"{"line": 2, "text": "fine now"}"#]).await;

        let c = commenter(server.uri(), None);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut ctx = remark_ctx(tmp.path(), vec![]);
        ctx.tool_rounds = 0;
        let remark = c.remark(&ctx, &cancel).await.unwrap().expect("a remark");
        assert_eq!(remark.text, "fine now", "{leaked}");

        let bodies = received_bodies(&server).await;
        assert_eq!(bodies.len(), 2, "{leaked}");
        for body in &bodies {
            assert!(
                body.get("tools").is_none(),
                "the retry offers no tools; server={}; requests={bodies:#?}",
                server.uri()
            );
        }
        let retry = user_messages(&bodies[1]);
        assert_eq!(
            retry
                .iter()
                .filter(|m| m.contains("No more tools are available"))
                .count(),
            2,
            "the instruction is repeated: {retry:?}"
        );
    }
}

// Given a provider that returns tool calls although none were offered
// When that is the final answer
// Then it is asked again, and the leaked call is never run.
#[tokio::test]
async fn provider_tool_calls_in_the_final_round_are_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    let server = isolated_server().await;
    let leaked = sse_stream(vec![sse_data(&delta_tool_call(
        "c",
        "read",
        &json!({"path": "a.rs"}),
    ))]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(leaked, "text/event-stream"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    script_texts(&server, &[r#"{"silent": true}"#]).await;

    let log_path = ws.join("nav.jsonl");
    let c = commenter(server.uri(), Some(log_path.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = remark_ctx(&ws, vec![]);
    ctx.tool_rounds = 0;
    assert!(c.remark(&ctx, &cancel).await.unwrap().is_none());
    assert_eq!(received_bodies(&server).await.len(), 2);
    let tools_run = NavLog::new(&log_path)
        .read_all()
        .iter()
        .filter(|r| r["layer"] == "metacognition_tool")
        .count();
    assert_eq!(tools_run, 0, "a leaked call is not executed");
}

// Given a model that tries a tool in both final answers
// When the metacognition remarks
// Then the result is `Malformed`, after exactly one retry.
#[tokio::test]
async fn two_tool_attempts_in_a_row_are_malformed() {
    use withnav::navigator::ports::ModelError;

    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    script_texts(&server, &[DSML_CALL, DSML_CALL, r#"{"silent": true}"#]).await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = remark_ctx(tmp.path(), vec![]);
    ctx.tool_rounds = 0;
    match c.remark(&ctx, &cancel).await {
        Err(ModelError::Malformed(raw)) => assert!(raw.contains("DSML"), "{raw}"),
        other => panic!("expected Malformed, got {other:?}"),
    }
    assert_eq!(
        received_bodies(&server).await.len(),
        2,
        "one retry, not more"
    );
}

// Given plain prose with no tool markup as the final answer
// When the metacognition remarks
// Then it is malformed at once, without a retry.
#[tokio::test]
async fn prose_without_tool_markup_is_not_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    script_texts(&server, &["just prose", r#"{"silent": true}"#]).await;

    let c = commenter(server.uri(), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    assert!(c
        .remark(&remark_ctx(tmp.path(), vec![]), &cancel)
        .await
        .is_err());
    assert_eq!(received_bodies(&server).await.len(), 1);
}

// Given a model that keeps calling tools
// When the context allows the help-call number of tool rounds, or the
//      ordinary one
// Then the help call gets three rounds with tools, an ordinary remark one.
#[tokio::test]
async fn a_help_remark_gets_three_tool_rounds_and_an_ordinary_one_one() {
    use withnav::navigator::metacognition::{HELP_TOOL_ROUNDS, REMARK_TOOL_ROUNDS};

    assert_eq!((HELP_TOOL_ROUNDS, REMARK_TOOL_ROUNDS), (3, 1));
    for (rounds, expected_requests) in [(HELP_TOOL_ROUNDS, 4), (REMARK_TOOL_ROUNDS, 2)] {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::write(ws.join("a.rs"), "a\n").unwrap();
        let server = isolated_server().await;
        let tool_round = sse_stream(vec![sse_data(&delta_tool_call(
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
        script_texts(&server, &[r#"{"silent": true}"#]).await;

        let c = commenter(server.uri(), None);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut ctx = remark_ctx(&ws, vec![]);
        ctx.tool_rounds = rounds;
        c.remark(&ctx, &cancel).await.unwrap();
        let bodies = received_bodies(&server).await;
        assert_eq!(bodies.len(), expected_requests, "rounds {rounds}");
        assert!(bodies.last().unwrap().get("tools").is_none());
    }
}

// Given a context carrying the rendered branch prompt, with its diff and
//      excerpt
// When the metacognition remarks
// Then the diff and the code excerpt reach the model exactly once, in the
//      system message, and the user turn is only the instruction to decide.
#[tokio::test]
async fn the_branch_context_is_sent_once() {
    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    script_texts(&server, &[r#"{"silent": true}"#]).await;

    let c = commenter(server.uri(), None);
    let mut ctx = remark_ctx(tmp.path(), vec![]);
    ctx.diff = "+UNIQUE_DIFF_MARKER".to_string();
    ctx.text_excerpt = "   2| UNIQUE_EXCERPT_MARKER".to_string();
    ctx.prompt = Some(format!(
        "Diff:\n{}\nContext:\n{}",
        ctx.diff, ctx.text_excerpt
    ));
    let cancel = tokio_util::sync::CancellationToken::new();
    c.remark(&ctx, &cancel).await.unwrap();

    let bodies = received_bodies(&server).await;
    let wire = bodies[0]["messages"].to_string();
    assert_eq!(wire.matches("UNIQUE_DIFF_MARKER").count(), 1, "{wire}");
    assert_eq!(wire.matches("UNIQUE_EXCERPT_MARKER").count(), 1, "{wire}");
    let users = user_messages(&bodies[0]);
    assert_eq!(users.len(), 1, "{users:?}");
    assert!(users[0].contains("JSON only"), "{users:?}");
    assert!(!users[0].contains("UNIQUE"), "{users:?}");
}

// Given no branch prompt (a direct caller)
// When the metacognition remarks
// Then the user turn still carries the diff and the excerpt.
#[tokio::test]
async fn without_a_branch_prompt_the_user_turn_carries_the_context() {
    let tmp = tempfile::tempdir().unwrap();
    let server = isolated_server().await;
    script_texts(&server, &[r#"{"silent": true}"#]).await;

    let c = commenter(server.uri(), None);
    let mut ctx = remark_ctx(tmp.path(), vec![]);
    ctx.diff = "+UNIQUE_DIFF_MARKER".to_string();
    let cancel = tokio_util::sync::CancellationToken::new();
    c.remark(&ctx, &cancel).await.unwrap();

    let bodies = received_bodies(&server).await;
    let users = user_messages(&bodies[0]);
    assert!(users[0].contains("UNIQUE_DIFF_MARKER"), "{users:?}");
}
