//! `request_secret` end to end: the agent asks, the Captain answers on the dedicated endpoint,
//! the value reaches a command's environment, and it never appears in a model request, a tool
//! result, the SSE stream or the stored conversation.
use reqwest::StatusCode;
use serde_json::{Value, json};

mod common;
use common::server::{TestServerConfig, create_session, post_message, spawn_server};
use common::sse::{
    http_sse, read_sse, spawn_capturing_provider, sse_body, text_chunk, tool_call_chunk,
};

const VALUE: &str = "sk-live-7f3a9c2e1d0b4a68";

fn tool_turn(id: &str, name: &str, args: Value) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_call_chunk(id, name, &args)]))
}

fn text_turn(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

fn request_secret() -> Vec<u8> {
    tool_turn(
        "sec_1",
        "request_secret",
        json!({"name":"API_KEY","label":"Deploy key","reason":"the staging deploy"}),
    )
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
async fn frames_until_turns(base: &str, id: &str, turns: usize) -> Vec<common::sse::SseFrame> {
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

fn payloads(frames: &[common::sse::SseFrame], kind: &str) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f.kind().as_deref() == Some(kind))
        .map(|f| serde_json::from_str::<Value>(&f.data).unwrap()["payload"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_reaches_the_command_and_never_the_model_the_stream_or_the_history() {
    let provider = spawn_capturing_provider(vec![
        request_secret(),
        tool_turn(
            "b1",
            "bash",
            json!({"command":"printf 'key=%s' \"$API_KEY\""}),
        ),
        text_turn("deployed"),
        text_turn("noted"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).await;

    assert_eq!(
        post_message(&base, &id, "deploy to staging").await.status(),
        200
    );
    let asked = frames_until_turns(&base, &id, 1).await;
    let yields = payloads(&asked, "agent.yield");
    assert_eq!(yields.len(), 1);
    assert_eq!(yields[0]["kind"], "secret");
    assert_eq!(yields[0]["question_call_id"], "sec_1");
    assert_eq!(yields[0]["details"]["name"], "API_KEY");
    assert_eq!(yields[0]["details"]["default_ttl"], "24h");
    assert!(
        payloads(&asked, "turn.end")
            .iter()
            .any(|p| p["outcome"] == "yielded"),
        "the conversation waits for the Captain, as on a question"
    );

    // A wrong name for the waiting request changes nothing.
    let wrong = post_secret(&base, &id, "sec_1", &json!({"name":"OTHER","value":VALUE})).await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(secrets(&base, &id).await["secrets"], json!([]));

    let saved = post_secret(
        &base,
        &id,
        "sec_1",
        &json!({"value":VALUE, "ttl":"1h", "scope":"session"}),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);
    let saved: Value = saved.json().await.unwrap();
    assert_eq!(saved["secret"]["name"], "API_KEY");
    assert_eq!(saved["secret"]["last4"], "4a68");
    assert_eq!(saved["secret"]["ttl"], "1h");
    assert!(!saved.to_string().contains(VALUE));

    // Settled: a second answer is stale.
    let again = post_secret(&base, &id, "sec_1", &json!({"decline":true})).await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(
        again.json::<Value>().await.unwrap()["reason"],
        "secret_stale"
    );

    let frames = frames_until_turns(&base, &id, 2).await;
    let posts = payloads(&frames, "tool.post_call");
    let bash = posts
        .iter()
        .find(|p| p["name"] == "bash")
        .expect("bash ran");
    assert_eq!(
        bash["output"], "key=[secret API_KEY]",
        "the command got the value"
    );
    let notes = payloads(&frames, "message.received");
    let note = notes
        .iter()
        .find(|p| !p["secret"].is_null())
        .expect("the answer is a secret note");
    assert_eq!(note["secret"]["outcome"], "stored");
    assert_eq!(note["secret"]["request_id"], "sec_1");
    assert!(
        note["content"]
            .as_str()
            .unwrap()
            .starts_with("stored as $API_KEY")
    );

    // The Captain pastes the value into the chat: it is hidden there too.
    assert_eq!(
        post_message(&base, &id, &format!("it was {VALUE}"))
            .await
            .status(),
        200
    );
    let frames = frames_until_turns(&base, &id, 3).await;
    assert!(
        payloads(&frames, "message.received")
            .iter()
            .any(|p| p["content"] == "it was [secret API_KEY]")
    );
    for frame in &frames {
        assert!(
            !frame.data.contains(VALUE),
            "SSE frame leaks: {}",
            frame.data
        );
    }

    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 4);
    for body in &bodies {
        assert!(!body.contains(VALUE), "a model request leaks the value");
    }
    assert!(bodies[1].contains("stored as $API_KEY"), "{}", bodies[1]);
    assert!(bodies[2].contains("key=[secret API_KEY]"), "{}", bodies[2]);

    let listed = secrets(&base, &id).await;
    assert_eq!(listed["secrets"][0]["use_count"], 1);
    assert_eq!(listed["secrets"][0]["scope"], "session");
    assert!(!listed.to_string().contains(VALUE));

    // The stored conversation: transcript and events.
    let store = withnav::session::store::HistoryStore::open(tmp.path().join("history.db")).unwrap();
    let session: ulid::Ulid = id.parse().unwrap();
    let stored =
        common::deadline::eventually("the secret exchange reaches the history", || async {
            let loaded = store.load(session).await.unwrap()?;
            let stored = format!(
                "{}{}",
                serde_json::to_string(&loaded.transcript).unwrap(),
                serde_json::to_string(&loaded.envelopes).unwrap()
            );
            (stored.contains("it was [secret API_KEY]") && stored.contains("noted"))
                .then_some(stored)
        })
        .await;
    assert!(stored.contains("stored as $API_KEY"));
    assert!(!stored.contains(VALUE), "the history leaks the value");

    // Revoke: the value is gone.
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/sessions/{id}/secrets/API_KEY");
    assert_eq!(
        client.delete(&url).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(secrets(&base, &id).await["secrets"], json!([]));
    assert_eq!(
        client.delete(&url).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_request_tells_the_model_and_a_bad_one_never_pauses() {
    let provider = spawn_capturing_provider(vec![
        tool_turn(
            "bad_1",
            "request_secret",
            json!({"name":"lower","label":"x","reason":"y"}),
        ),
        request_secret(),
        text_turn("continuing without it"),
        text_turn("thanks"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .allow_everything()
        .state();
    let (addr, _task) = spawn_server(state).await;
    let base = format!("http://{addr}");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id = create_session(&base, &ws).await;
    assert_eq!(post_message(&base, &id, "go").await.status(), 200);
    let asked = frames_until_turns(&base, &id, 1).await;
    let yields = payloads(&asked, "agent.yield");
    assert_eq!(yields.len(), 1, "only the valid request pauses");
    assert_eq!(yields[0]["question_call_id"], "sec_1");

    let declined = post_secret(&base, &id, "sec_1", &json!({"decline":true})).await;
    assert_eq!(declined.status(), StatusCode::OK);
    let frames = frames_until_turns(&base, &id, 2).await;
    assert!(
        payloads(&frames, "message.received")
            .iter()
            .any(|p| p["secret"]["outcome"] == "declined")
    );
    let bodies = provider.bodies();
    assert!(bodies[1].contains("invalid secret name"), "{}", bodies[1]);
    assert!(
        bodies[2].contains("declined by the Captain"),
        "{}",
        bodies[2]
    );

    // Provide anyway: stored once the request is settled, by name.
    let later = post_secret(
        &base,
        &id,
        "sec_1",
        &json!({"name":"API_KEY","value":VALUE,"scope":"workspace"}),
    )
    .await;
    assert_eq!(later.status(), StatusCode::OK);
    let listed = secrets(&base, &id).await;
    assert_eq!(listed["secrets"][0]["scope"], "workspace");
    let file = tmp.path().join("data/credentials/secrets.json");
    assert!(file.exists(), "a workspace secret is saved");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
