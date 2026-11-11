//! `request_secret` end to end: the agent asks, the Captain answers on the dedicated endpoint,
//! the value reaches a command's environment, and it never appears in a model request, a tool
//! result, the SSE stream or the stored conversation.
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
    // the reply to it are there, in the transcript and in the events.
    let store = withnav::session::store::HistoryStore::open(tmp.path().join("history.db")).unwrap();
    let pasted = "it was [secret API_KEY]";
    let session: ulid::Ulid = id.parse().unwrap();
    let loaded = common::deadline::eventually(
        "the pasted message and the reply to it, in the stored transcript and events",
        || async {
            let loaded = store.load(session).await.unwrap()?;
            let in_transcript = loaded
                .transcript
                .iter()
                .any(|m| matches!(m, ChatMessage::User { content, .. } if content == pasted))
                && loaded.transcript.iter().any(|m| {
                    matches!(m, ChatMessage::Assistant { content: Some(text), .. } if text == "noted")
                });
            let in_events = loaded
                .envelopes
                .iter()
                .any(|e| e.kind == "message.received" && e.payload["content"] == pasted)
                && loaded
                    .envelopes
                    .iter()
                    .any(|e| e.kind == "llm.response" && e.payload["message"]["text"] == "noted");
            (in_transcript && in_events).then_some(loaded)
        },
    )
    .await;
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
    // A server-side failure, reported without the OS error or the value.
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value = failed.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "error": "the secret could not be saved; see the server log",
            "reason": "secret_not_saved",
        })
    );

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

// Lifecycle and scope. Every turn, the ones a "provided a secret" note starts included, takes
// the next scripted response, so each step waits for its `turn.end` before the next one.

/// The names these tests store. Commands inherit the test process's environment, so a name
/// already set there would pass for an injected secret.
const NAMES: [&str; 4] = [
    "LIFECYCLE_KEY",
    "RUN_LONG_KEY",
    "DAY_LONG_KEY",
    "PROVIDED_KEY",
];

fn bash_turn(id: &str, command: &str) -> Vec<u8> {
    tool_turn(id, "bash", json!({ "command": command }))
}

/// "Provide anyway": a value for `name` given outside a waiting request.
async fn provide(base: &str, id: &str, name: &str, value: &str, ttl: &str, scope: &str) -> Value {
    let response = post_secret(
        base,
        id,
        "provided",
        &json!({"name": name, "value": value, "ttl": ttl, "scope": scope}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

/// What each `bash` call printed, in order.
fn bash_outputs(frames: &[common::sse::SseFrame]) -> Vec<String> {
    payloads(frames, "tool.post_call")
        .iter()
        .filter(|p| p["name"] == "bash")
        .map(|p| p["output"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// What the tool call `call_id` returned, once turn number `turn` ended.
async fn call_output(base: &str, id: &str, turn: usize, call_id: &str) -> String {
    let frames = frames_until_turns(base, id, turn).await;
    payloads(&frames, "tool.post_call")
        .into_iter()
        .find(|p| p["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("{call_id} did not run"))["output"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The secrets a session lists, as `(name, scope)`.
async fn listed(base: &str, id: &str) -> Vec<(String, String)> {
    secrets(base, id).await["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_owned(),
                s["scope"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn revoke(base: &str, id: &str, name: &str) -> StatusCode {
    reqwest::Client::new()
        .delete(format!("{base}/v1/sessions/{id}/secrets/{name}"))
        .send()
        .await
        .unwrap()
        .status()
}

struct Served {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    ws: std::path::PathBuf,
    base: String,
    state: withnav::server::AppState,
    _task: tokio::task::JoinHandle<()>,
}

async fn serve(provider: &str) -> Served {
    serve_with(provider, |config| config, |_| {}).await
}

/// `tune` adjusts the test configuration; `prepare` edits the written settings before the
/// server starts (writing the configuration again would overwrite them).
async fn serve_with(
    provider: &str,
    tune: impl FnOnce(TestServerConfig) -> TestServerConfig,
    prepare: impl FnOnce(&withnav::server::ServerConfig),
) -> Served {
    for name in NAMES {
        assert!(
            std::env::var_os(name).is_none(),
            "${name} is set for the tests"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let config = tune(TestServerConfig::mock(&root, provider).allow_everything()).config();
    prepare(&config);
    let state = withnav::server::AppState::new(config).unwrap();
    let (addr, task) = spawn_server(state.clone()).await;
    Served {
        _tmp: tmp,
        root,
        ws,
        base: format!("http://{addr}"),
        state,
        _task: task,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_kept_while_the_conversation_runs_ends_when_it_stops() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        text_turn("noted"),
        bash_turn(
            "b1",
            "printf '%s|%s' \"${RUN_LONG_KEY:-unset}\" \"${DAY_LONG_KEY:-unset}\"",
        ),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let id = create_session(&fx.base, &fx.ws).await;
    provide(
        &fx.base,
        &id,
        "RUN_LONG_KEY",
        "session-long-value-1",
        "session",
        "session",
    )
    .await;
    frames_until_turns(&fx.base, &id, 1).await;
    provide(
        &fx.base,
        &id,
        "DAY_LONG_KEY",
        "day-long-value-0001",
        "24h",
        "session",
    )
    .await;
    frames_until_turns(&fx.base, &id, 2).await;
    assert_eq!(
        listed(&fx.base, &id).await,
        [
            ("DAY_LONG_KEY".into(), "session".into()),
            ("RUN_LONG_KEY".into(), "session".into())
        ]
    );

    let stop = reqwest::Client::new()
        .post(format!("{}/v1/sessions/{id}/stop", fx.base))
        .send()
        .await
        .unwrap();
    assert_eq!(stop.status(), StatusCode::OK);
    let resumed =
        common::server::post_create_session(&fx.base, &json!({"workspace": fx.ws, "resume": id}))
            .await;
    assert!(resumed.status().is_success(), "{}", resumed.status());
    let resumed: Value = resumed.json().await.unwrap();
    assert_eq!(resumed["session_id"], id.as_str());

    // Only the secret given "for as long as the conversation runs" went with the stop.
    assert_eq!(
        listed(&fx.base, &id).await,
        [("DAY_LONG_KEY".to_owned(), "session".to_owned())]
    );
    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    assert_eq!(
        call_output(&fx.base, &id, 3, "b1").await,
        "unset|[secret DAY_LONG_KEY]"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_secret_reaches_its_own_workspace_only() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
        bash_turn("b2", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let other_ws = fx.root.join("other");
    std::fs::create_dir_all(&other_ws).unwrap();

    let owner = create_session(&fx.base, &fx.ws).await;
    provide(&fx.base, &owner, "LIFECYCLE_KEY", VALUE, "24h", "workspace").await;
    frames_until_turns(&fx.base, &owner, 1).await;

    let elsewhere = create_session(&fx.base, &other_ws).await;
    assert_eq!(listed(&fx.base, &elsewhere).await, []);
    assert_eq!(
        post_message(&fx.base, &elsewhere, "check").await.status(),
        200
    );
    assert_eq!(call_output(&fx.base, &elsewhere, 1, "b1").await, "unset");

    let neighbour = create_session(&fx.base, &fx.ws).await;
    assert_eq!(
        listed(&fx.base, &neighbour).await,
        [("LIFECYCLE_KEY".to_owned(), "workspace".to_owned())]
    );
    assert_eq!(
        post_message(&fx.base, &neighbour, "check").await.status(),
        200
    );
    assert_eq!(
        call_output(&fx.base, &neighbour, 1, "b2").await,
        "[secret LIFECYCLE_KEY]"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subagent_command_gets_the_conversations_secret_and_its_output_is_redacted() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", "printf 'key=%s' \"$LIFECYCLE_KEY\""),
        text_turn("child done"),
        text_turn("parent noted the result"),
    ])
    .await;
    let fx = serve_with(&provider.url, |config| config.chat_tools(128_000), |_| {}).await;
    let agents = fx.ws.join(".withnav/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("runner.md"),
        "---\nmode: subagent\naccess: full\n---\nRun the requested command.",
    )
    .unwrap();
    let root = create_session(&fx.base, &fx.ws).await;
    provide(&fx.base, &root, "LIFECYCLE_KEY", VALUE, "24h", "session").await;
    frames_until_turns(&fx.base, &root, 1).await;

    let child = fx
        .state
        .create_child_session(root.parse().unwrap(), "runner", "print the key")
        .await
        .unwrap()
        .to_string();
    let frames = frames_until_turns(&fx.base, &child, 1).await;
    assert_eq!(bash_outputs(&frames), ["key=[secret LIFECYCLE_KEY]"]);
    for frame in &frames {
        assert!(
            !frame.data.contains(VALUE),
            "child event leaks: {}",
            frame.data
        );
    }
    // The child's result wakes the root for one more turn.
    frames_until_turns(&fx.base, &root, 2).await;
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 4);
    for body in &bodies {
        assert!(!body.contains(VALUE), "a model request leaks the value");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_server_never_receives_a_conversations_secret() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        tool_turn("c1", "mcp__fake__crash", json!({})),
        text_turn("it crashed"),
        tool_turn("m1", "mcp__fake__environ", json!({"text": "LIFECYCLE_KEY"})),
        text_turn("checked"),
    ])
    .await;
    let fx = serve_with(
        &provider.url,
        |config| config,
        |config| {
            let config_dir = &config.config_dir;
            let mut settings = withnav::settings::load(config_dir).unwrap();
            settings.set_dotted(
                "mcp",
                json!({"fake": {
                    "command": [
                        common::python(),
                        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py"),
                    ],
                    "env": {"TOOL_NAMES": "[\"environ\", \"crash\"]", "ANNOUNCE_START": "1"},
                }}),
            );
            withnav::settings::save(config_dir, &settings).unwrap();
        },
    )
    .await;
    // The server's stderr: the log a client reads in the server status.
    let tail = || {
        fx.state
            .service()
            .mcp_statuses()
            .into_iter()
            .find(|s| s.server_id == "fake" && s.state == "ready")
            .map(|s| s.stderr_tail)
            .unwrap_or_default()
    };
    let started = |tail: &str| -> Vec<String> {
        tail.lines()
            .filter_map(|line| line.strip_prefix("started "))
            .map(str::to_owned)
            .collect()
    };
    common::server::wait_for("MCP discovery", || !started(&tail()).is_empty()).await;
    let first = started(&tail());
    let id = create_session(&fx.base, &fx.ws).await;
    provide(&fx.base, &id, "LIFECYCLE_KEY", VALUE, "24h", "session").await;
    frames_until_turns(&fx.base, &id, 1).await;

    // Restarted once the secret exists: a process started now must not get it either.
    assert_eq!(
        post_message(&fx.base, &id, "restart it").await.status(),
        200
    );
    frames_until_turns(&fx.base, &id, 2).await;
    common::server::wait_for("the MCP server's restart", || {
        started(&tail()).len() > first.len()
    })
    .await;
    let pid = started(&tail()).pop().unwrap();

    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    assert_eq!(
        call_output(&fx.base, &id, 3, "m1").await,
        format!("{pid}:unset"),
        "the restarted MCP process has no $LIFECYCLE_KEY"
    );
    let done = format!("environ done {pid}");
    common::server::wait_for("the environment on stderr", || tail().contains(&done)).await;
    let tail = tail();
    assert!(!tail.contains("LIFECYCLE_KEY"), "{tail}");
    assert!(!tail.contains(VALUE), "{tail}");
}

#[cfg(debug_assertions)] // the clock hook exists in debug builds only
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_secret_is_neither_listed_nor_given_to_commands() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
        bash_turn("b2", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let id = create_session(&fx.base, &fx.ws).await;
    provide(&fx.base, &id, "LIFECYCLE_KEY", VALUE, "1h", "session").await;
    frames_until_turns(&fx.base, &id, 1).await;
    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    assert_eq!(
        call_output(&fx.base, &id, 2, "b1").await,
        "[secret LIFECYCLE_KEY]"
    );

    fx.state.service().advance_secret_clock_test(3_500);
    assert_eq!(listed(&fx.base, &id).await.len(), 1, "not yet expired");
    fx.state.service().advance_secret_clock_test(101);
    // The command first: a listing would drop the expired entry before it runs.
    assert_eq!(post_message(&fx.base, &id, "again").await.status(), 200);
    assert_eq!(call_output(&fx.base, &id, 3, "b2").await, "unset");
    assert_eq!(listed(&fx.base, &id).await, []);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_secret_hides_the_workspace_one_until_it_is_revoked() {
    // Values are redacted to the same `[secret LIFECYCLE_KEY]`: the command prints the prefix.
    let prefix = "printf '%s' \"${LIFECYCLE_KEY%%-*}\"";
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", prefix),
        text_turn("checked"),
        bash_turn("b2", prefix),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let id = create_session(&fx.base, &fx.ws).await;
    provide(
        &fx.base,
        &id,
        "LIFECYCLE_KEY",
        "work-value-0001",
        "24h",
        "workspace",
    )
    .await;
    frames_until_turns(&fx.base, &id, 1).await;
    // The name is known now: no note, no turn.
    provide(
        &fx.base,
        &id,
        "LIFECYCLE_KEY",
        "sess-value-0001",
        "24h",
        "session",
    )
    .await;
    assert_eq!(
        listed(&fx.base, &id).await,
        [("LIFECYCLE_KEY".to_owned(), "session".to_owned())]
    );
    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    assert_eq!(call_output(&fx.base, &id, 2, "b1").await, "sess");

    assert_eq!(
        revoke(&fx.base, &id, "LIFECYCLE_KEY").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        listed(&fx.base, &id).await,
        [("LIFECYCLE_KEY".to_owned(), "workspace".to_owned())]
    );
    assert_eq!(post_message(&fx.base, &id, "again").await.status(), 200);
    assert_eq!(call_output(&fx.base, &id, 3, "b2").await, "work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_secret_no_longer_reaches_new_commands() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
        bash_turn("b2", "printf '%s' \"${LIFECYCLE_KEY:-unset}\""),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let id = create_session(&fx.base, &fx.ws).await;
    provide(&fx.base, &id, "LIFECYCLE_KEY", VALUE, "24h", "session").await;
    frames_until_turns(&fx.base, &id, 1).await;
    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    assert_eq!(
        call_output(&fx.base, &id, 2, "b1").await,
        "[secret LIFECYCLE_KEY]"
    );

    assert_eq!(
        revoke(&fx.base, &id, "LIFECYCLE_KEY").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(post_message(&fx.base, &id, "again").await.status(), 200);
    assert_eq!(call_output(&fx.base, &id, 3, "b2").await, "unset");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provide_anyway_announces_a_new_name_and_silently_replaces_a_known_one() {
    let provider = spawn_capturing_provider(vec![
        text_turn("noted"),
        bash_turn("b1", "printf '%s' \"${PROVIDED_KEY%%-*}\""),
        text_turn("checked"),
    ])
    .await;
    let fx = serve(&provider.url).await;
    let id = create_session(&fx.base, &fx.ws).await;
    let first = provide(
        &fx.base,
        &id,
        "PROVIDED_KEY",
        "first-value-0001",
        "24h",
        "session",
    )
    .await;
    assert_eq!(first["secret"]["last4"], "0001");
    frames_until_turns(&fx.base, &id, 1).await;

    let second = provide(
        &fx.base,
        &id,
        "PROVIDED_KEY",
        "second-value-002",
        "24h",
        "session",
    )
    .await;
    assert_eq!(second["secret"]["last4"], "-002");
    let listed = secrets(&fx.base, &id).await;
    assert_eq!(listed["secrets"].as_array().unwrap().len(), 1);
    assert_eq!(listed["secrets"][0]["last4"], "-002");

    assert_eq!(post_message(&fx.base, &id, "check").await.status(), 200);
    let frames = frames_until_turns(&fx.base, &id, 2).await;
    assert_eq!(
        bash_outputs(&frames),
        ["second"],
        "the new value replaced the old"
    );
    let notes: Vec<Value> = payloads(&frames, "message.received")
        .into_iter()
        .filter(|p| !p["secret"].is_null())
        .collect();
    assert_eq!(notes.len(), 1, "only the new name is announced: {notes:?}");
    assert_eq!(notes[0]["secret"]["outcome"], "stored");
    assert_eq!(notes[0]["secret"]["name"], "PROVIDED_KEY");
    assert!(
        notes[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("The Captain provided a secret: stored as $PROVIDED_KEY"),
        "{}",
        notes[0]["content"]
    );
}
