//! `request_secret` end to end: the agent asks, the Captain answers on the dedicated endpoint,
//! the value reaches a command's environment, and it never appears in a model request, a tool
//! result, the SSE stream or the stored conversation.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;

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

fn turn_ends(frames: &[common::sse::SseFrame]) -> usize {
    frames
        .iter()
        .filter(|f| f.kind().as_deref() == Some("turn.end"))
        .count()
}

/// Reads the whole stream from the start until `turn.end` number `turns` arrived; panics
/// when the stream stops first, so a missing turn never passes as an empty one.
async fn frames_until_turns(base: &str, id: &str, turns: usize) -> Vec<common::sse::SseFrame> {
    let read = read_sse(base, id, None, |frames| turn_ends(frames) >= turns).await;
    assert!(
        turn_ends(&read.frames) >= turns,
        "expected {turns} turn.end, the stream stopped ({:?}) after {}",
        read.end,
        turn_ends(&read.frames)
    );
    read.frames
}

/// A running server over a test root, with one conversation in `<root>/ws`.
struct Server {
    base: String,
    state: AppState,
    task: tokio::task::JoinHandle<()>,
    ws: std::path::PathBuf,
    id: String,
}

impl Server {
    async fn start(config: &TestServerConfig, root: &std::path::Path) -> Self {
        let state = config.state();
        let (addr, task) = spawn_server(state.clone()).await;
        let base = format!("http://{addr}");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let id = create_session(&base, &ws).await;
        Self {
            base,
            state,
            task,
            ws,
            id,
        }
    }

    /// Stops this server and starts a new one over the same data folder and history
    /// database, with a new conversation in the same workspace: what it sees is what
    /// reached the disk.
    async fn restart(self, config: &TestServerConfig) -> Self {
        self.state.close_all().await;
        self.task.abort();
        let _ = self.task.await;
        drop(self.state);
        let state = config.state();
        let (addr, task) = spawn_server(state.clone()).await;
        let base = format!("http://{addr}");
        let id = create_session(&base, &self.ws).await;
        Self {
            base,
            state,
            task,
            ws: self.ws,
            id,
        }
    }
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
    // Writes reach the store asynchronously: wait, bounded, until both the pasted message and
    // the reply to it are there, in the transcript and in the events, then assert each one.
    let store = withnav::session::store::HistoryStore::open(tmp.path().join("history.db")).unwrap();
    let pasted = "it was [secret API_KEY]";
    let in_transcript = |loaded: &withnav::session::store::LoadedSession| {
        let user = loaded
            .transcript
            .iter()
            .any(|m| matches!(m, ChatMessage::User { content, .. } if content == pasted));
        let reply = loaded.transcript.iter().any(
            |m| matches!(m, ChatMessage::Assistant { content: Some(text), .. } if text == "noted"),
        );
        (user, reply)
    };
    let in_events = |loaded: &withnav::session::store::LoadedSession| {
        let user = loaded
            .envelopes
            .iter()
            .any(|e| e.kind == "message.received" && e.payload["content"] == pasted);
        let reply = loaded
            .envelopes
            .iter()
            .any(|e| e.kind == "llm.response" && e.payload["message"]["text"] == "noted");
        (user, reply)
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let loaded = loop {
        let loaded = store.load(id.parse().unwrap()).await.unwrap().unwrap();
        let done = in_transcript(&loaded) == (true, true) && in_events(&loaded) == (true, true);
        if done || tokio::time::Instant::now() >= deadline {
            break loaded;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (user, reply) = in_transcript(&loaded);
    assert!(user, "the pasted message is not in the stored transcript");
    assert!(reply, "the reply to it is not in the stored transcript");
    let (user, reply) = in_events(&loaded);
    assert!(user, "the pasted message is not in the stored events");
    assert!(reply, "the reply to it is not in the stored events");
    let stored = format!(
        "{}{}",
        serde_json::to_string(&loaded.transcript).unwrap(),
        serde_json::to_string(&loaded.envelopes).unwrap()
    );
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
    let config = TestServerConfig::mock(tmp.path(), &provider.url).allow_everything();
    let server = Server::start(&config, tmp.path()).await;
    let (base, id) = (server.base.clone(), server.id.clone());
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
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(
            mode(file.parent().unwrap()),
            0o700,
            "the credentials folder"
        );
    }

    // A restarted server reads it back: a new conversation of the workspace sees it.
    let server = server.restart(&config).await;
    let listed = secrets(&server.base, &server.id).await;
    assert_eq!(listed["secrets"][0]["name"], "API_KEY");
    assert_eq!(listed["secrets"][0]["scope"], "workspace");
    assert_eq!(listed["secrets"][0]["last4"], "4a68");
}

/// A live workspace secret as `secrets.json` (version 1) stores it.
fn saved_secret(ws: &std::path::Path) -> Value {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    json!({
        "name": "API_KEY",
        "value": VALUE,
        "workspace": ws.canonicalize().unwrap(),
        "ttl": "24h",
        "created_at": now,
        "expires_at": now + 3600,
        "use_count": 0,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_secret_that_cannot_be_saved_is_reported_and_not_kept_across_a_restart() {
    let provider = spawn_capturing_provider(vec![request_secret(), text_turn("without it")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url).allow_everything();
    // A file where the credentials folder goes: the secrets file cannot be written.
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("credentials"), "").unwrap();
    let server = Server::start(&config, tmp.path()).await;
    let (base, id) = (server.base.clone(), server.id.clone());
    assert_eq!(post_message(&base, &id, "deploy").await.status(), 200);
    frames_until_turns(&base, &id, 1).await;

    let failed = post_secret(
        &base,
        &id,
        "sec_1",
        &json!({"value":VALUE, "ttl":"24h", "scope":"workspace"}),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    let error = failed.text().await.unwrap();
    assert!(error.contains("could not be written"), "{error}");
    assert!(!error.contains(VALUE));

    // The request was not settled as stored: it still waits, and a decline answers it.
    let declined = post_secret(&base, &id, "sec_1", &json!({"decline":true})).await;
    assert_eq!(declined.status(), StatusCode::OK);
    let frames = frames_until_turns(&base, &id, 2).await;
    let outcomes: Vec<Value> = payloads(&frames, "message.received")
        .iter()
        .map(|p| p["secret"]["outcome"].clone())
        .filter(|o| !o.is_null())
        .collect();
    assert_eq!(outcomes, vec![json!("declined")]);

    let server = server.restart(&config).await;
    assert_eq!(
        secrets(&server.base, &server.id).await["secrets"],
        json!([])
    );
}

/// Gives a folder back its 0700 mode when dropped: a failing assertion after a `chmod 000`
/// otherwise leaves a temporary folder that cannot be removed.
#[cfg(unix)]
struct OpenOnDrop(std::path::PathBuf);

#[cfg(unix)]
impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secrets_file_that_cannot_be_read_is_left_untouched_at_start() {
    // Each case: a name, the file's version (none: not JSON), whether the credentials folder
    // is made unreadable. Versions 1 and 2 hold a live secret of the workspace.
    let cases = [
        ("damaged", None, false),
        ("unknown version", Some(2), false),
        ("unreadable", Some(1), true),
    ];
    for (case, version, unreadable) in cases {
        if unreadable && !cfg!(unix) {
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let config = TestServerConfig::unreachable(tmp.path());
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = tmp.path().join("data").join("credentials");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secrets.json");
        let bytes = match version {
            Some(version) => {
                json!({"version": version, "secrets": [saved_secret(&ws)]}).to_string()
            }
            None => "{ not json".to_owned(),
        };
        std::fs::write(&file, &bytes).unwrap();
        #[cfg(unix)]
        let _restore = unreadable.then(|| OpenOnDrop(dir.clone()));
        #[cfg(unix)]
        if unreadable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read(&file).is_ok() {
                // A privileged user reads through the mode: the case proves nothing there.
                continue;
            }
        }
        let server = Server::start(&config, tmp.path()).await;
        assert_eq!(
            secrets(&server.base, &server.id).await["secrets"],
            json!([]),
            "{case}: nothing is loaded"
        );
        server.state.close_all().await;
        server.task.abort();
        #[cfg(unix)]
        if unreadable {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o000, "{case}: the folder is left as it was");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            bytes,
            "{case}: the file is left as it was"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_secret_use_counts_reach_the_file_with_the_next_save() {
    let provider = spawn_capturing_provider(vec![
        request_secret(),
        tool_turn("b1", "bash", json!({"command":"test -n \"$API_KEY\""})),
        text_turn("deployed"),
        text_turn("noted"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = TestServerConfig::mock(tmp.path(), &provider.url).allow_everything();
    let server = Server::start(&config, tmp.path()).await;
    let (base, id) = (server.base.clone(), server.id.clone());
    assert_eq!(post_message(&base, &id, "deploy").await.status(), 200);
    frames_until_turns(&base, &id, 1).await;
    let saved = post_secret(
        &base,
        &id,
        "sec_1",
        &json!({"value":VALUE, "ttl":"24h", "scope":"workspace"}),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);
    let frames = frames_until_turns(&base, &id, 2).await;
    assert!(
        payloads(&frames, "tool.post_call")
            .iter()
            .any(|p| p["name"] == "bash"),
        "the command ran with the secret"
    );
    assert_eq!(secrets(&base, &id).await["secrets"][0]["use_count"], 1);

    // The next save (another workspace secret) writes the count with the rest.
    let other = post_secret(
        &base,
        &id,
        "later",
        &json!({"name":"OTHER_KEY","value":"other-value-1234","ttl":"24h","scope":"workspace"}),
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    frames_until_turns(&base, &id, 3).await;

    let server = server.restart(&config).await;
    let listed = secrets(&server.base, &server.id).await;
    let api_key = listed["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "API_KEY")
        .cloned()
        .expect("API_KEY survived the restart");
    assert_eq!(api_key["use_count"], 1, "{listed}");
}
