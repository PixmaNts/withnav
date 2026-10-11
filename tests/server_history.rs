//! Conversation history (navigator, server half): persistence at the HTTP
//! seam with a scripted provider — create with `history_db` set, converse,
//! list, resume in a FRESH AppState on the same database file, the resumed
//! model request contains the earlier messages, the SSE stream of the
//! resumed session replays the stored events; delete; the list is
//! per-workspace; a test config never names the user's data dir.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::navigator::COGNITION_PROMPT;
use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;

mod common;
use common::server::{
    collect_until, list_sessions, post_create_session, post_message_ok, spawn_server,
    TestServerConfig,
};
use common::sse::{spawn_capturing_provider, text_response as done_response, CapturingProvider};

/// A state over a `cognition`-aliased mock provider at `base_url`, with
/// history persisted to `history_db`.
fn make_state(tmp: &Path, base_url: &str, history_db: PathBuf) -> AppState {
    TestServerConfig::mock(tmp, base_url)
        .model_alias("cognition", "mock-opus")
        .history_db(history_db)
        .assigned_navigator_state()
}

/// Waits for the writer's acknowledged barrier, then verifies the HTTP list.
async fn wait_for_list(state: &AppState, base: &str, workspace: &Path, min: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        state.history().flush().await.expect("flush");
        let body = list_sessions(&reqwest::Client::new(), base, workspace).await;
        let sessions = body["sessions"].as_array().expect("sessions").clone();
        assert!(
            sessions.len() >= min,
            "expected {min} sessions: {sessions:?}"
        );
        sessions
    })
    .await
    .expect("history writer and HTTP list must finish")
}

// ---- the acceptance tests ----

struct ServerA {
    base: String,
    state: AppState,
    ws: PathBuf,
    _mock: CapturingProvider,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ServerA {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The tempdir is returned SEPARATELY: a resume test drops the first server
/// (and its state) while keeping the database file on disk.
async fn spawn_history_server() -> (ServerA, PathBuf, tempfile::TempDir) {
    let mock = spawn_capturing_provider(vec![done_response("OK"); 2]).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db = root.join("sessions.db");
    let state = make_state(&root, &mock.url, db.clone());
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    (
        ServerA {
            base: format!("http://{addr}"),
            state,
            ws: ws.clone(),
            _mock: mock,
            task,
        },
        db,
        tmp,
    )
}

// Given a session created with history_db set against a scripted provider
// When a message completes its turn
// Then the store has the session with a title from the first captain message,
//      the events are stored (minus llm.delta/session.end), and the
//      transcript snapshot is saved
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn converse_persists_events_title_and_transcript() {
    let (server, db, _keep) = spawn_history_server().await;
    let created = post_create_session(
        &server.base,
        &json!({ "workspace": server.ws, "editor": true }),
    )
    .await;
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await.expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();

    let sub = server
        .state
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub");
    let msg = "Remember the word: pineapple<editor-context>{\"file\":\"a.rs\"}</editor-context>";
    post_message_ok(&server.base, &id, msg).await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"),
        "turn completed"
    );

    let listed = wait_for_list(&server.state, &server.base, &server.ws, 1).await;
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(listed[0]["title"], "Remember the word: pineapple");
    assert_eq!(listed[0]["message_count"], 1);
    assert_eq!(listed[0]["live"], true);

    // Direct store probe: events stored except llm.delta; no session.end
    // while live; transcript snapshot present.
    let store = withnav::session::store::HistoryStore::open(&db).unwrap();
    let loaded = tokio::time::timeout(Duration::from_secs(5), store.load(id.parse().unwrap()))
        .await
        .expect("load")
        .expect("store load")
        .expect("row");
    let kinds: Vec<&str> = loaded.envelopes.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"session.start"));
    assert!(kinds.contains(&"message.received"));
    assert!(kinds.contains(&"llm.response"));
    assert!(kinds.contains(&"turn.end"));
    assert!(!kinds.contains(&"llm.delta"), "deltas are never stored");
    assert!(!kinds.contains(&"session.end"), "no session.end stored");
    assert!(
        !loaded.transcript.is_empty(),
        "transcript snapshot after turn end"
    );
    assert!(
        !loaded
            .transcript
            .iter()
            .any(|m| matches!(m, ChatMessage::System { .. })),
        "the navigator role prompt is not stored in the transcript"
    );
}

// Given a persisted conversation in one AppState
// When a FRESH AppState opens the same database file and resumes the id
// Then the SAME id is returned, the resumed model request contains the
//      earlier messages, and the SSE stream replays the stored events
//      before any new one (no new session.start)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_in_a_fresh_state_replays_and_remembers() {
    let (server, db, _keep) = spawn_history_server().await;
    let created = post_create_session(
        &server.base,
        &json!({ "workspace": server.ws, "editor": true }),
    )
    .await;
    let created: Value = created.json().await.expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();

    // Turn 1 in the ORIGINAL state.
    let sub = server
        .state
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub");
    post_message_ok(&server.base, &id, "Remember the word: pineapple").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let original_seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let original_max = *original_seqs.iter().max().expect("seqs");
    wait_for_list(&server.state, &server.base, &server.ws, 1).await;
    // Kill the first server (drop state: its runtime tasks go with it).
    // The tempdir stays alive through `_keep`: the db file must survive.
    server.task.abort();
    drop(server);

    // Turn 2 in a FRESH AppState over the same file.
    let mock2 = spawn_capturing_provider(vec![done_response("OK, the word was pineapple")]).await;
    let tmp2 = tempfile::tempdir().expect("tempdir 2");
    let root2 = tmp2.path().to_path_buf();
    // Same workspace path content-wise: reuse a copy of the same ws string.
    let ws2 = root2.join("ws");
    std::fs::create_dir_all(&ws2).expect("ws2");
    let state2 = make_state(&root2, &mock2.url, db.clone());
    // The two scripted providers listen on different ports, and a resume
    // refuses another endpoint: the stored row is moved to the second one so
    // this test stays about what a resume replays.
    common::server::move_saved_endpoint(&db, &id, &mock2.url);
    // The resume keys on the STORED row (workspace/model/editor are the
    // stored ones); the request's workspace only guides models.json lookup.
    let (resumed_id, resumed) = state2
        .create_or_resume_session(&ws2, None, false, Some(id.parse().unwrap()))
        .await
        .expect("resume");
    assert_eq!(resumed_id.to_string(), id, "same id returned");
    assert!(resumed, "flagged as resumed");
    let (addr2, task2) = spawn_server(state2.clone()).await;
    let base2 = format!("http://{addr2}");

    // A fresh subscriber (no cursor) sees the WHOLE past conversation.
    let sub2 = state2
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub2");
    if let withnav::bus::ResumeOutcome::Replay(replay) = &sub2.replay {
        assert!(
            replay.iter().any(|e| e.kind == "session.start"),
            "the original session.start replays"
        );
        assert!(
            replay.iter().any(|e| e.kind == "message.received"),
            "the original captain message replays"
        );
        assert!(
            !replay.iter().any(|e| e.kind == "session.end"),
            "a resumed stream must not look closed"
        );
        let replay_max = replay.iter().map(|e| e.seq).max().unwrap_or(0);
        // The resume wrote one event of its own (bypass starts off); the
        // stored ones keep their seqs.
        assert!(
            replay_max <= original_max + 1,
            "stored seqs preserved: {replay_max} <= {original_max} + 1"
        );
        assert!(replay
            .iter()
            .any(|e| e.kind == "session.bypass" && e.payload["by"] == "system:resume"));
    } else {
        panic!("expected replay, got {:?}", sub2.replay);
    }

    // Drive the next turn: the model request must contain the earlier
    // messages (the remembered word).
    post_message_ok(&base2, &id, "What word did I ask you to remember?").await;
    let events2 = collect_until(sub2, |e| e.kind == "turn.end").await;
    assert!(events2.iter().any(|e| e.payload["outcome"] == "done"));
    let new_events: Vec<&EventEnvelope> = events2.iter().filter(|e| e.seq > original_max).collect();
    assert!(!new_events.is_empty(), "new events were sequenced");
    let new_min = new_events.iter().map(|e| e.seq).min().unwrap();
    assert_eq!(
        new_min,
        original_max + 1,
        "the resumed sequencer continues at last_seq + 1 (new {new_min}, stored max {original_max})"
    );
    assert!(
        !new_events.iter().any(|e| e.kind == "session.start"),
        "NO new session.start on resume"
    );
    let new_message = new_events
        .iter()
        .find(|e| e.kind == "message.received")
        .expect("the new captain message was sequenced after the stored head");
    assert_eq!(
        new_message.payload["content"],
        "What word did I ask you to remember?"
    );

    // The provider saw the earlier conversation (it records each request
    // before answering, and the turn has ended).
    let requests = mock2.bodies();
    assert!(!requests.is_empty(), "provider called");
    let body: Value = serde_json::from_str(requests.last().unwrap()).expect("request json");
    let contents: Vec<String> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        contents.iter().any(|c| c.contains("pineapple")),
        "the earlier messages ride along: {contents:?}"
    );
    task2.abort();
}

// Given a session that completed a turn and was then stopped
// When it is resumed (same state, after the stop) and driven again
// Then every new event's seq is above the old session.end seq, and the
//      replayed history does not contain the old session.end
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_continues_after_the_stored_session_end() {
    let (server, _db, _keep) = spawn_history_server().await;
    let created: Value = post_create_session(
        &server.base,
        &json!({ "workspace": server.ws, "editor": true }),
    )
    .await
    .json()
    .await
    .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let ulid: ulid::Ulid = id.parse().unwrap();

    let sub = server.state.subscribe(ulid, None).await.expect("sub");
    post_message_ok(&server.base, &id, "first turn").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;

    // Stop it, observing the real session.end.
    let sub = server.state.subscribe(ulid, None).await.expect("sub");
    server.state.close_session(ulid).await.expect("close");
    let closed = collect_until(sub, |e| e.kind == "session.end").await;
    let end_seq = closed
        .iter()
        .find(|e| e.kind == "session.end")
        .expect("session.end observed")
        .seq;

    let (_, resumed) = server
        .state
        .create_or_resume_session(&server.ws, None, false, Some(ulid))
        .await
        .expect("resume");
    assert!(resumed);
    let sub = server.state.subscribe(ulid, None).await.expect("sub");
    if let withnav::bus::ResumeOutcome::Replay(replay) = &sub.replay {
        assert!(
            !replay.iter().any(|e| e.kind == "session.end"),
            "the old session.end is not replayed to the resumed stream"
        );
    } else {
        panic!("expected replay");
    }
    post_message_ok(&server.base, &id, "second turn").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let from_second: Vec<&EventEnvelope> = events
        .iter()
        .skip_while(|e| !(e.kind == "message.received" && e.payload["content"] == "second turn"))
        .collect();
    assert!(
        from_second.len() >= 2,
        "the second turn produced events: {from_second:?}"
    );
    for event in &from_second {
        assert!(
            event.seq > end_seq,
            "{} seq {} must be above the old session.end seq {end_seq}",
            event.kind,
            event.seq
        );
    }
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seqs strictly increase: {seqs:?}"
    );
}

/// The messages of the last request the mock provider saw.
fn last_request_messages(mock: &CapturingProvider) -> Vec<Value> {
    let requests = mock.bodies();
    let body: Value =
        serde_json::from_str(requests.last().expect("provider called")).expect("request json");
    body["messages"].as_array().expect("messages").clone()
}

// Given a navigator transcript stored before the role prompt left the
//      transcript (it starts with the prompt as a system message)
// When the session is resumed and a message is posted
// Then the model request carries the loop contract and the role prompt
//      exactly once each, followed by the stored turns and the new message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_an_old_navigator_transcript_sends_the_role_prompt_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db = root.join("sessions.db");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws");

    let id = ulid::Ulid::new();
    let store = withnav::session::store::HistoryStore::open(&db).expect("open");
    store
        .record_session(id, &ws.display().to_string(), "mock/mock-opus", true)
        .await
        .expect("record");
    store.append_event(
        id,
        EventEnvelope::new(id, 0, EventKind::SessionStart, json!({})),
    );
    store.save_transcript(
        id,
        vec![
            ChatMessage::System {
                content: COGNITION_PROMPT.to_string(),
            },
            ChatMessage::User {
                provenance: Default::default(),
                content: "earlier question".into(),
            },
            ChatMessage::Assistant {
                content: Some("earlier answer".into()),
                reasoning_content: None,
                tool_calls: vec![],
            },
        ],
    );
    store.flush().await.expect("flush");

    let mock = spawn_capturing_provider(vec![done_response("fine")]).await;
    let state = make_state(&root, &mock.url, db.clone());
    let (resumed_id, resumed) = state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resume");
    assert_eq!(resumed_id, id);
    assert!(resumed);
    let (addr, task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let sub = state.subscribe(id, None).await.expect("sub");
    post_message_ok(&base, &id.to_string(), "new question").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;

    let messages = last_request_messages(&mock);
    let shape: Vec<(&str, &str)> = messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or_default(),
                m["content"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(shape.len(), 5, "{shape:?}");
    assert_eq!(shape[0].0, "system");
    assert_eq!(shape[1].0, "system");
    assert!(shape[1].1.starts_with("You are Nav · The Navigator"));
    assert!(shape[1].1.ends_with(COGNITION_PROMPT));
    assert_eq!(shape[2], ("user", "earlier question"));
    assert_eq!(shape[3], ("assistant", "earlier answer"));
    assert_eq!(shape[4], ("user", "new question"));
    task.abort();
}

// Given a live session and its history
// When DELETE /v1/sessions/{id} runs
// Then 204, the store no longer lists it, and a resume of that id 404s
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_from_memory_and_store() {
    let (server, _db, _keep) = spawn_history_server().await;
    let created: Value = post_create_session(
        &server.base,
        &json!({ "workspace": server.ws, "editor": true }),
    )
    .await
    .json()
    .await
    .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let sub = server
        .state
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub");
    post_message_ok(&server.base, &id, "hello").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;
    wait_for_list(&server.state, &server.base, &server.ws, 1).await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{id}", server.base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(matches!(
        server.state.session_summary(id.parse().unwrap()).await,
        Err(withnav::error::ServerError::SessionNotFound(_))
    ));

    // Store is empty for the workspace after the writer acknowledges deletion.
    let client = reqwest::Client::new();
    let body = tokio::time::timeout(Duration::from_secs(10), async {
        server
            .state
            .history()
            .flush()
            .await
            .expect("flush deletion");
        list_sessions(&client, &server.base, &server.ws).await
    })
    .await
    .expect("deleted history list must finish");
    assert_eq!(body["sessions"].as_array().map(|a| a.len()), Some(0));

    // A disk-only id that never existed 404s.
    let bogus = ulid::Ulid::new().to_string();
    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{bogus}", server.base))
        .send()
        .await
        .expect("delete bogus");
    assert_eq!(resp.status(), 404);

    // Resuming the deleted id 404s too.
    let resp = post_create_session(
        &server.base,
        &json!({ "workspace": server.ws, "resume": id }),
    )
    .await;
    assert_eq!(resp.status(), 404);
}

// Given two workspaces with different conversations
// When GET /v1/sessions?workspace=... runs for each
// Then each list only contains its own workspace's sessions
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_is_per_workspace() {
    let (server, _db, _keep) = spawn_history_server().await;
    let other = server.ws.parent().unwrap().join("other ws & # + %");
    std::fs::create_dir_all(&other).expect("other ws");

    for (ws, text) in [
        (&server.ws, "in the first workspace"),
        (&other, "in the second"),
    ] {
        let created: Value =
            post_create_session(&server.base, &json!({ "workspace": ws, "editor": true }))
                .await
                .json()
                .await
                .expect("json");
        let id = created["session_id"].as_str().expect("id").to_string();
        let sub = server
            .state
            .subscribe(id.parse().unwrap(), None)
            .await
            .expect("sub");
        post_message_ok(&server.base, &id, text).await;
        let _ = collect_until(sub, |e| e.kind == "turn.end").await;
    }

    let first = wait_for_list(&server.state, &server.base, &server.ws, 1).await;
    assert_eq!(first.len(), 1, "only the first workspace's session");
    assert_eq!(first[0]["title"], "in the first workspace");
    let second = wait_for_list(&server.state, &server.base, &other, 1).await;
    assert_eq!(second.len(), 1, "only the second workspace's session");
    assert_eq!(second[0]["title"], "in the second");
}

// Given a test server config that names no history database
// When it is built and a conversation turn completes
// Then the database sits in the test's own temp root and the user's real
//      data dir is never named, so no test can write there
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_server_config_never_resolves_to_the_users_data_dir() {
    let mock = spawn_capturing_provider(vec![done_response("OK")]).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let config = TestServerConfig::mock(&root, &mock.url);
    let db = config.config().history_db;
    assert_eq!(db, root.join("history.db"));
    if let Some(data_dir) = withnav::paths::data_dir() {
        assert!(
            !db.starts_with(&data_dir),
            "{db:?} is in the user's data dir"
        );
    }

    let state = config.state();
    let (addr, task) = spawn_server(state.clone()).await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    let base = format!("http://{addr}");
    let created: Value = post_create_session(&base, &json!({ "workspace": ws }))
        .await
        .json()
        .await
        .expect("json");
    let id = created["session_id"].as_str().expect("id").to_string();
    let sub = state
        .subscribe(id.parse().unwrap(), None)
        .await
        .expect("sub");
    post_message_ok(&base, &id, "persisted in the temp root").await;
    let _ = collect_until(sub, |e| e.kind == "turn.end").await;

    let body = wait_for_list(&state, &base, &ws, 1).await;
    assert_eq!(body.len(), 1, "the conversation is listed from the temp db");
    assert!(db.exists(), "the database is the one in the temp root");
    task.abort();
}

// Given two sessions of one workspace, one with an unreadable agent_overrides column
// When GET /v1/sessions?workspace=... runs
// Then the healthy session is listed and the unreadable one is left out, not a 500
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_skips_a_session_with_unreadable_metadata() {
    let (server, db, _keep) = spawn_history_server().await;
    let healthy = server
        .state
        .create_session(&server.ws, None, false)
        .await
        .expect("healthy");
    let broken = server
        .state
        .create_session(&server.ws, None, false)
        .await
        .expect("broken");
    server.state.history().flush().await.expect("flush");
    rusqlite::Connection::open(&db)
        .expect("db")
        .execute(
            "UPDATE sessions SET agent_overrides='invalid json' WHERE id=?1",
            [broken.to_string()],
        )
        .expect("corrupt");

    let body = list_sessions(&reqwest::Client::new(), &server.base, &server.ws).await;
    let ids: Vec<&str> = body["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec![healthy.to_string()]);
}
