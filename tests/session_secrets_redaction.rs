//! Secrets redaction boundaries: live stream delta redaction, provider request and history redaction,
//! tool result and error redaction, submission parameter validations, persistence scope separation,
//! and character-count threshold behavior.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};

mod common;
use common::deadline::Deadline;
use common::server::{TestServerConfig, create_session, post_message, spawn_server, stored_events};
use common::sse::{
    SseEnd, SseFrame, http_sse, read_sse, spawn_capturing_provider, spawn_streaming_provider,
    sse_body, text_chunk, tool_call_chunk,
};
use withnav::provider::openai::ChatMessage;

const VALUE: &str = "sk-live-7f3a9c2e1d0b4a68";

fn tool_turn(id: &str, name: &str, args: Value) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_call_chunk(id, name, &args)]))
}

fn text_turn(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

async fn post_secret(base: &str, id: &str, request: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/secrets/{request}"))
        .json(body)
        .send()
        .await
        .expect("post secret")
}

async fn secrets(base: &str, id: &str) -> Value {
    reqwest::get(format!("{base}/v1/sessions/{id}/secrets"))
        .await
        .expect("list secrets")
        .json()
        .await
        .expect("secrets json")
}

/// Reads the whole stream from the start until `turn.end` number `turns` arrived.
async fn frames_until_turns(base: &str, id: &str, turns: usize) -> Vec<SseFrame> {
    read_sse(base, id, None, |frames| {
        frames
            .iter()
            .filter(|f| f.kind().as_deref() == Some("turn.end"))
            .count()
            >= turns
    })
    .await
    .frames
}

fn payloads(frames: &[SseFrame], kind: &str) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f.kind().as_deref() == Some(kind))
        .map(|f| serde_json::from_str::<Value>(&f.data).unwrap()["payload"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_llm_deltas_are_redacted_on_the_sse_stream() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;

    // Storing a secret on an idle session dispatches a host note that drives a full agent turn
    // (src/session/secrets.rs provide_later). The note turn's streamed response is the redaction subject;
    // the delta redaction path in the agent loop is identical for every turn source.
    let saved = post_secret(
        &base,
        &id,
        "init",
        &json!({"name":"API_KEY", "value":VALUE}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(saved.status(), StatusCode::OK);

    // Live SSE deltas are never replayed (the sequencer's live queue is registered after the replay
    // snapshot), so the subscriber must connect before the turn runs to observe llm.delta frames.
    let base_clone = base.clone();
    let id_clone = id.clone();
    let reader = tokio::spawn(async move {
        read_sse(&base_clone, &id_clone, None, |frames| {
            frames
                .iter()
                .any(|f| f.kind().as_deref() == Some("turn.end"))
        })
        .await
    });

    // Allow the SSE connection to establish and attach to the sequencer's live broadcast channel.
    tokio::time::sleep(Duration::from_millis(150))
        .bounded(file!(), line!())
        .await;

    let (_request, chunks) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .expect("provider request arrived");

    // Put the whole secret value in one text chunk so per-chunk delta redaction catches it.
    chunks.send(text_chunk(VALUE)).expect("send delta chunk");
    drop(chunks);

    let read = reader
        .bounded(file!(), line!())
        .await
        .expect("join reader task");
    assert_eq!(read.end, SseEnd::Until, "turn.end must conclude the stream");

    assert!(
        read.frames
            .iter()
            .any(|f| f.kind().as_deref() == Some("llm.delta")),
        "at least one llm.delta frame must arrive"
    );

    for frame in &read.frames {
        assert!(
            !frame.data.contains(VALUE),
            "live SSE frame leaks the secret: {}",
            frame.data
        );
    }

    assert!(
        read.frames
            .iter()
            .any(|f| f.data.contains("[secret API_KEY]")),
        "some frame data must contain the redacted placeholder"
    );

    assert!(
        read.frames
            .iter()
            .any(|f| f.kind().as_deref() == Some("turn.end")),
        "turn.end frame arrived"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_response_and_transcript_are_redacted() {
    let provider = spawn_capturing_provider(vec![text_turn(VALUE), text_turn("done")])
        .bounded(file!(), line!())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state.clone()).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;

    // Storing a secret on an idle session dispatches a host note that drives turn 1.
    // The note turn's scripted response is the one that outputs VALUE.
    let saved = post_secret(
        &base,
        &id,
        "init",
        &json!({"name":"API_KEY", "value":VALUE}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(saved.status(), StatusCode::OK);

    // Follow-up message prompts the model again; its provider request must carry the redacted history.
    assert_eq!(
        post_message(&base, &id, "follow-up message")
            .bounded(file!(), line!())
            .await
            .status(),
        StatusCode::OK
    );
    let frames = frames_until_turns(&base, &id, 2)
        .bounded(file!(), line!())
        .await;

    // Stored llm.response event payload contains "[secret API_KEY]" and not VALUE.
    let responses = payloads(&frames, "llm.response");
    assert!(!responses.is_empty(), "llm.response event must be present");
    let response_text = serde_json::to_string(&responses[0]).unwrap();
    assert!(
        response_text.contains("[secret API_KEY]"),
        "llm.response payload must contain redacted placeholder: {response_text}"
    );
    assert!(
        !response_text.contains(VALUE),
        "llm.response payload leaks the secret value: {response_text}"
    );

    // Conversation transcript in the history store contains "[secret API_KEY]" and not VALUE.
    let session_ulid = id.parse::<ulid::Ulid>().unwrap();
    let loaded = state
        .history()
        .load(session_ulid)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .expect("loaded session");
    let transcript_text = serde_json::to_string(&loaded.transcript).unwrap();
    assert!(
        transcript_text.contains("[secret API_KEY]"),
        "history transcript must contain redacted placeholder: {transcript_text}"
    );
    assert!(
        !transcript_text.contains(VALUE),
        "history transcript leaks the secret value: {transcript_text}"
    );

    // Check captured provider request bodies.
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 2, "two provider requests captured");
    assert!(
        !bodies[0].contains(VALUE),
        "first request body must not leak the secret value: {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("[secret API_KEY]"),
        "second request body must contain redacted placeholder: {}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains(VALUE),
        "second request body must not leak the secret value: {}",
        bodies[1]
    );

    for frame in &frames {
        assert!(
            !frame.data.contains(VALUE),
            "SSE frame leaks secret: {}",
            frame.data
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_metadata_and_tool_errors_are_redacted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();

    let fixture_task = tokio::spawn(async move {
        let responses = [
            format!("HTTP/1.1 302 Found\r\nLocation: /{VALUE}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ];
        for resp in responses {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, &resp).await;
                let _ = tokio::io::AsyncWriteExt::flush(&mut sock).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
            }
        }
    });

    let provider = spawn_capturing_provider(vec![
        tool_turn(
            "f1",
            "fetch",
            json!({"url": format!("http://127.0.0.1:{port}/start")}),
        ),
        tool_turn(
            "f2",
            "fetch",
            json!({"url": format!("http://127.0.0.1:{port}/{VALUE}")}),
        ),
        text_turn("done"),
    ])
    .bounded(file!(), line!())
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state.clone()).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;

    // Storing a secret on an idle session dispatches a host note that drives a full agent turn
    // (src/session/secrets.rs provide_later). That note turn runs the scripted fetch tool chain:
    // the first call follows a same-origin redirect populating metadata.url, the second triggers
    // a 404 populating error.message and output.
    let saved = post_secret(
        &base,
        &id,
        "init",
        &json!({"name":"API_KEY", "value":VALUE}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(saved.status(), StatusCode::OK);

    let frames = frames_until_turns(&base, &id, 1)
        .bounded(file!(), line!())
        .await;

    let posts = payloads(&frames, "tool.post_call");
    let f1 = posts
        .iter()
        .find(|p| p["tool_call_id"] == "f1")
        .expect("f1 post_call");
    assert_eq!(f1["metadata"]["redirects"], 1);
    let f1_url = f1["metadata"]["url"].as_str().expect("f1 url");
    assert!(
        f1_url.contains("[secret API_KEY]"),
        "redirect target url in metadata must be redacted: {f1_url}"
    );
    assert!(
        !f1_url.contains(VALUE),
        "metadata url must not leak secret: {f1_url}"
    );

    let f2 = posts
        .iter()
        .find(|p| p["tool_call_id"] == "f2")
        .expect("f2 post_call");
    let f2_output = f2["output"].as_str().expect("f2 output");
    assert!(
        f2_output.contains("[secret API_KEY]"),
        "error output must be redacted: {f2_output}"
    );
    assert!(
        !f2_output.contains(VALUE),
        "error output must not leak secret: {f2_output}"
    );

    // Stored transcript tool result verification
    let session_ulid = id.parse::<ulid::Ulid>().unwrap();
    let loaded = state
        .history()
        .load(session_ulid)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .expect("session loaded");

    let f2_tool_msg = loaded
        .transcript
        .iter()
        .find(|m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "f2"))
        .expect("f2 tool message in transcript");

    if let ChatMessage::Tool { content, .. } = f2_tool_msg {
        let parsed: Value = serde_json::from_str(content).unwrap();
        let err_msg = parsed["error"]["message"].as_str().expect("error.message");
        assert!(
            err_msg.contains("[secret API_KEY]"),
            "error.message in transcript must be redacted: {err_msg}"
        );
        assert!(
            !err_msg.contains(VALUE),
            "error.message in transcript must not leak secret: {err_msg}"
        );
        let out = parsed["output"].as_str().expect("output");
        assert!(
            out.contains("[secret API_KEY]"),
            "output in transcript must be redacted: {out}"
        );
        assert!(
            !out.contains(VALUE),
            "output in transcript must not leak secret: {out}"
        );
    }

    // Persisted tool.post_call events
    let stored = stored_events(&state, session_ulid, "tool.post_call")
        .bounded(file!(), line!())
        .await;
    assert_eq!(stored.len(), 2);
    for event in stored {
        let s = serde_json::to_string(&event.payload).unwrap();
        assert!(!s.contains(VALUE), "stored event payload leaks secret: {s}");
    }

    for frame in frames
        .iter()
        .filter(|f| f.kind().as_deref() == Some("tool.post_call"))
    {
        assert!(
            !frame.data.contains(VALUE),
            "tool.post_call frame leaks secret: {}",
            frame.data
        );
    }

    for body in provider.bodies() {
        assert!(
            !body.contains(VALUE),
            "provider request body leaks secret: {body}"
        );
    }

    fixture_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submission_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path())
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;

    // 1. Reserved name -> 400
    let resp = post_secret(&base, &id, "any", &json!({"name":"PATH","value":"x"}))
        .bounded(file!(), line!())
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().bounded(file!(), line!()).await.unwrap();
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("reserved"),
        "error must mention reserved: {err}"
    );

    // 2. Bad name regex -> 400
    let resp = post_secret(&base, &id, "any", &json!({"name":"bad-name","value":"x"}))
        .bounded(file!(), line!())
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().bounded(file!(), line!()).await.unwrap();
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("^[A-Z][A-Z0-9_]{1,63}$"),
        "error must mention regex expectation: {err}"
    );

    // 3. GOOD_KEY with value shorter than 8 chars -> 200, last4 == ""
    let resp = post_secret(
        &base,
        &id,
        "any",
        &json!({"name":"GOOD_KEY","value":"short"}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().bounded(file!(), line!()).await.unwrap();
    assert_eq!(body["secret"]["last4"], "");
    assert!(
        !body.to_string().contains("short"),
        "full response JSON must never contain the raw secret value: {body}"
    );

    // 4. decline without pending request -> 409, reason == "secret_stale"
    let resp = post_secret(&base, &id, "any", &json!({"decline":true}))
        .bounded(file!(), line!())
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = resp.json().bounded(file!(), line!()).await.unwrap();
    assert_eq!(body["reason"], "secret_stale");

    // 5. value without name and without pending request -> 409
    let resp = post_secret(&base, &id, "any", &json!({"value":"x"}))
        .bounded(file!(), line!())
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // 6. GET /v1/sessions/{id}/secrets lists GOOD_KEY with last4 "" and use_count 0
    let listed = secrets(&base, &id).bounded(file!(), line!()).await;
    let list = listed["secrets"].as_array().expect("secrets array");
    let good = list
        .iter()
        .find(|s| s["name"] == "GOOD_KEY")
        .expect("GOOD_KEY listed");
    assert_eq!(good["last4"], "");
    assert_eq!(good["use_count"], 0);
    assert!(!listed.to_string().contains("short"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_scope_secrets_never_touch_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path())
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;

    let secrets_file = tmp.path().join("data/credentials/secrets.json");

    let session_val = "session-sec-1234";
    let resp = post_secret(
        &base,
        &id,
        "any",
        &json!({
            "name": "SESSION_KEY",
            "value": session_val,
            "ttl": "session",
            "scope": "session"
        }),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !secrets_file.exists(),
        "session-scope secret must not write secrets.json to disk"
    );

    let workspace_val = "workspac-sec-567";
    let resp = post_secret(
        &base,
        &id,
        "any",
        &json!({
            "name": "WORKSPACE_KEY",
            "value": workspace_val,
            "ttl": "1h",
            "scope": "workspace"
        }),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        secrets_file.exists(),
        "workspace-scope secret must write secrets.json to disk"
    );

    let raw_file = std::fs::read_to_string(&secrets_file).unwrap();
    let parsed: Value = serde_json::from_str(&raw_file).unwrap();
    let entries = parsed["secrets"].as_array().expect("secrets array");
    assert!(
        entries.iter().any(|e| e["name"] == "WORKSPACE_KEY"),
        "WORKSPACE_KEY must be in secrets.json"
    );
    assert!(
        !entries.iter().any(|e| e["name"] == "SESSION_KEY"),
        "SESSION_KEY must NOT be in secrets.json"
    );
    assert!(
        raw_file.contains(workspace_val),
        "persisted secrets.json must contain workspace value to reload after restart"
    );
    assert!(
        !raw_file.contains(session_val),
        "session secret value must never appear in secrets.json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_values_flow_through_unredacted() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted secret"),
        text_turn("noted short"),
        text_turn("noted wide secret"),
        text_turn("noted wide"),
    ])
    .bounded(file!(), line!())
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state.clone()).bounded(file!(), line!()).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).bounded(file!(), line!()).await;
    let session_ulid = id.parse::<ulid::Ulid>().unwrap();

    // 1. Store 7-character secret (below the 8-character redaction threshold).
    // provide_later delivers a host note command that drives turn 1.
    let short_val = "SHORT7X";
    assert_eq!(short_val.chars().count(), 7);
    let resp = post_secret(
        &base,
        &id,
        "req1",
        &json!({"name":"SHORT_KEY", "value":short_val}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = frames_until_turns(&base, &id, 1)
        .bounded(file!(), line!())
        .await;

    // Paste the short value in chat: turn 2.
    assert_eq!(
        post_message(&base, &id, &format!("it was {short_val}"))
            .bounded(file!(), line!())
            .await
            .status(),
        StatusCode::OK
    );
    let _ = frames_until_turns(&base, &id, 2)
        .bounded(file!(), line!())
        .await;

    // Provider request body for turn 2 contains "SHORT7X" verbatim (NOT redacted)
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[1].contains("SHORT7X"),
        "short value under 8 chars must not be redacted in provider request: {}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains("[secret SHORT_KEY]"),
        "short value must not produce a placeholder"
    );

    // Stored transcript and events contain "SHORT7X" verbatim
    let loaded = state
        .history()
        .load(session_ulid)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .expect("loaded session");
    let transcript_text = serde_json::to_string(&loaded.transcript).unwrap();
    assert!(
        transcript_text.contains("SHORT7X"),
        "transcript must contain SHORT7X: {transcript_text}"
    );
    assert!(
        !transcript_text.contains("[secret SHORT_KEY]"),
        "transcript must not contain [secret SHORT_KEY]"
    );

    // 2. Multibyte value of exactly 8 characters (16 bytes in UTF-8).
    // Storing it drives turn 3.
    let wide_val = "éééééééé";
    assert_eq!(wide_val.chars().count(), 8);
    assert_eq!(wide_val.len(), 16);

    let resp = post_secret(
        &base,
        &id,
        "req2",
        &json!({"name":"WIDE_KEY", "value":wide_val}),
    )
    .bounded(file!(), line!())
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = frames_until_turns(&base, &id, 3)
        .bounded(file!(), line!())
        .await;

    // Paste the wide value in chat: turn 4.
    assert_eq!(
        post_message(&base, &id, &format!("it was {wide_val}"))
            .bounded(file!(), line!())
            .await
            .status(),
        StatusCode::OK
    );
    let _ = frames_until_turns(&base, &id, 4)
        .bounded(file!(), line!())
        .await;

    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 4);
    assert!(
        bodies[3].contains("[secret WIDE_KEY]"),
        "8-char multibyte secret must be redacted: {}",
        bodies[3]
    );
    assert!(
        !bodies[3].contains(wide_val),
        "multibyte secret value must not leak into provider request: {}",
        bodies[3]
    );

    let loaded = state
        .history()
        .load(session_ulid)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .expect("loaded session");
    let transcript_text = serde_json::to_string(&loaded.transcript).unwrap();
    assert!(
        transcript_text.contains("[secret WIDE_KEY]"),
        "transcript must contain [secret WIDE_KEY]: {transcript_text}"
    );
    assert!(
        !transcript_text.contains(wide_val),
        "transcript must not leak multibyte secret: {transcript_text}"
    );
}
