//! Integration tests: the Axum surface over a REAL HTTP client where
//! it matters, `tower::ServiceExt::oneshot` for pure routing. Frozen bodies
//! as documented in `server::routes`. Sessions are created against a test models.json whose
//! provider points at an unreachable mock base_url — creation must NEVER
//! call the provider (the actor is built but not run until a message is
//! posted).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use navai::server::{app, AppState, ServerConfig};

mod common;
use common::server::{request_json, text_command, TestServerConfig};

/// SIGTERM is PROCESS-GLOBAL: the real-signal test must not fire while
/// another test's server (which installs its own shutdown_signal handler)
/// is running. Both real-socket tests hold this lock.
static REAL_SOCKET_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn test_config(tmp: &Path) -> ServerConfig {
    TestServerConfig::unreachable(tmp).config()
}

fn test_state(tmp: &Path) -> AppState {
    TestServerConfig::unreachable(tmp).state()
}

#[tokio::test]
async fn editor_is_explicit_saved_context_and_resume_rejects_selection_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let state = test_state(tmp.path());
    let router = app(state.clone());
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    for body in [
        json!({"workspace": ws, "profile": "navigator"}),
        json!({"workspace": ws, "editor": "true"}),
        json!({"workspace": ws, "unexpected": true}),
    ] {
        let (status, _) = request_json(&router, "POST", "/v1/sessions", Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    for editor in [false, true] {
        let body = if editor {
            json!({"workspace": ws, "editor": true})
        } else {
            json!({"workspace": ws})
        };
        let (status, body) = request_json(&router, "POST", "/v1/sessions", Some(&body)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id: ulid::Ulid = body["session_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            state.history().meta(id).await.unwrap().unwrap().editor,
            editor
        );
        let fresh = state.run_command(id, "new", "").await.unwrap();
        assert!(fresh.ok, "{fresh:?}");
        let fresh_id: ulid::Ulid = fresh.data.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            state
                .history()
                .meta(fresh_id)
                .await
                .unwrap()
                .unwrap()
                .editor,
            editor
        );
        for conflicting in [
            json!({"editor": false}),
            json!({"editor": true}),
            json!({"agent": "build"}),
            json!({"model": "default"}),
            json!({"variant": null}),
        ] {
            let mut body = conflicting;
            body["workspace"] = json!(ws);
            body["resume"] = json!(id.to_string());
            let (status, _) = request_json(&router, "POST", "/v1/sessions", Some(&body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        state.close_session(id).await.unwrap();
        let (status, body) = request_json(
            &router,
            "POST",
            "/v1/sessions",
            Some(&json!({"workspace": ws, "resume": id.to_string()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            state.history().meta(id).await.unwrap().unwrap().editor,
            editor
        );
    }
    state.close_all().await;
}

async fn create_session(state: &AppState, workspace: &str, model: Option<&str>) -> Value {
    let (status, body) = request_json(
        &app(state.clone()),
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": workspace, "model": model })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

// Given the mounted router
// When GET /v1/health
// Then 200 with the frozen {status:"ok"} body
#[tokio::test]
async fn health_returns_200_ok_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (status, value) =
        request_json(&app(test_state(tmp.path())), "GET", "/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value, json!({ "status": "ok" }));
}

// Given a test models.json (dead provider base_url)
// When POST /v1/sessions
// Then 200 with the frozen create body; the provider is NEVER called at
//      creation (the actor is built but not run until a message is posted)
#[tokio::test]
async fn create_session_returns_frozen_body_without_calling_the_provider() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path());
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");

    let body = create_session(&state, ws.to_str().expect("utf8 ws"), None).await;
    assert_eq!(body["root_actor_id"], "agent:main");
    assert_eq!(body["actor_state"], "idle");
    assert!(body["session_id"].is_string(), "ulid session id: {body}");
}

// Given a created session
// When GET /v1/sessions/:id
// Then 200 with the frozen summary and current_seq == 0 (only session.start
//      was sequenced)
#[tokio::test]
async fn get_session_reports_current_seq_zero_right_after_create() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path());
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");

    let created = create_session(&state, ws.to_str().expect("utf8 ws"), None).await;
    let id = created["session_id"].as_str().expect("session id");

    let (status, value) = request_json(
        &app(state.clone()),
        "GET",
        &format!("/v1/sessions/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["session_id"], id);
    assert_eq!(value["actor_state"], "idle");
    assert_eq!(value["workspace"], ws.to_str().expect("utf8 ws"));
    assert_eq!(value["model"], "mock/mock-model");
    assert_eq!(value["current_seq"], 0);
}

// Given an id that was never minted (and a non-ULID string)
// When GET /v1/sessions/:id
// Then a structured 404 {"error": ...}
#[tokio::test]
async fn unknown_session_id_is_a_structured_404() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path());
    let app = app(state);

    for id in ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "not-a-ulid"] {
        let (status, value) = request_json(&app, "GET", &format!("/v1/sessions/{id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "id={id}");
        assert!(value.get("error").is_some(), "structured 404 body: {value}");
    }
}

// Given a connect-then-drop / create-then-get loop
// When the loop runs 20 rounds
// Then the registry size stays exactly the created-session count: no client
//      traffic can grow it (registry hygiene)
#[tokio::test]
async fn registry_does_not_grow_from_connect_then_drop_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = Arc::new(test_state(tmp.path()));
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");

    let rounds = 20;
    let mut ids = Vec::new();
    for _ in 0..rounds {
        let created = create_session(&state, ws.to_str().expect("utf8 ws"), None).await;
        ids.push(created["session_id"].as_str().expect("id").to_string());

        let state_for_get = (*state).clone();
        let id = ids.last().expect("id").clone();
        let get_task = tokio::spawn(async move {
            let (status, _) = request_json(
                &app(state_for_get),
                "GET",
                &format!("/v1/sessions/{id}"),
                None,
            )
            .await;
            status
        });
        let got = get_task.await.expect("get task");
        assert_eq!(got, StatusCode::OK);
        // ...and the GET "connection" is dropped right here (oneshot ends).
    }

    assert_eq!(
        state.session_count(),
        rounds,
        "registry holds exactly the created sessions, nothing more"
    );
}

// Given serve() on an ephemeral loopback port over a REAL socket
// When health and create/get are driven via reqwest
// Then the real-server path answers identically (acceptance: real HTTP)
#[tokio::test]
async fn real_socket_serve_answers_health_create_and_get() {
    let _socket_guard = REAL_SOCKET_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let bound = navai::server::routes::serve_on(
        ServerConfig {
            bearer_token: None,
            bind: addr,
            ..config
        },
        listener,
    );

    let server = tokio::spawn(bound);
    common::server::wait_until_healthy(addr).await;
    let base = format!("http://{addr}");

    let health: Value = reqwest::get(format!("{base}/v1/health"))
        .await
        .expect("health")
        .json()
        .await
        .expect("health json");
    assert_eq!(health, json!({ "status": "ok" }));

    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/v1/sessions"))
        .json(&json!({ "workspace": ws.to_str().expect("utf8 ws") }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("create json");
    assert_eq!(created["root_actor_id"], "agent:main");
    let id = created["session_id"].as_str().expect("id");

    let summary: Value = client
        .get(format!("{base}/v1/sessions/{id}"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("get json");
    assert_eq!(summary["current_seq"], 0);
    assert_eq!(summary["session_id"], id);

    server.abort();
}

// Given a created session closed through the manager
// When GET asks for it afterwards
// Then it answers "stopped" with session.end's seq from the store, is NOT
//      routable, and a second close is idempotent (no second session.end)
#[tokio::test]
async fn close_session_removes_the_entry_and_get_answers_from_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path());
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");

    let created = create_session(&state, ws.to_str().expect("utf8 ws"), None).await;
    let id: navai::bus::command::SessionId = created["session_id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("ulid");

    // THE TEST (not the manager) subscribes to observe session.end.
    let sub = state
        .subscribe(id, None)
        .await
        .expect("subscribe before close");

    state.close_session(id).await.expect("close");

    let mut seqs = Vec::new();
    let mut end_kind: Option<String> = None;
    if let navai::bus::ResumeOutcome::Replay(replay) = &sub.replay {
        seqs.extend(replay.iter().map(|e| e.seq));
        end_kind = replay.last().map(|e| e.kind.clone());
    }
    if let Some(mut live) = sub.live {
        while let Some(env) = tokio::time::timeout(std::time::Duration::from_secs(5), live.recv())
            .await
            .expect("drain within timeout")
        {
            seqs.push(env.seq);
            end_kind = Some(env.kind);
        }
    }
    let end_kind = end_kind.expect("at least session.end observed");
    assert_eq!(end_kind, "session.end");
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "gapless terminal stream: {seqs:?}"
    );

    // The store answers GET with session.end's seq; routing is gone.
    let summary = state.session_summary(id).await.expect("store answers");
    assert_eq!(summary.actor_state, "stopped");
    assert_eq!(summary.current_seq, *seqs.last().expect("seqs"));
    assert_not_routable(&state, id).await;

    state.close_session(id).await.expect("idempotent close");
    assert_eq!(state.session_count(), 0, "the registry holds no entry");
    let again = state.session_summary(id).await.expect("still answers");
    assert_eq!(
        again.current_seq, summary.current_seq,
        "no second session.end"
    );
}

// Given two active sessions and a REAL SIGTERM delivered to a server running
// the true graceful-shutdown path (with_graceful_shutdown + shutdown_signal)
// When the signal fires
// Then each session's stream ends with session.end{final_seq}, and serve
//      RETURNS CLEANLY (the server task completes; socket closed)
//
// Mechanism: the test drives the REAL shutdown_signal future — it spawns it
// and sends an OS SIGTERM to the test process (the same handler axum's
// graceful shutdown awaits; in-process SIGTERM delivery is reliable on this
// platform and flake-free because the signal handler is process-wide and the
// server task is the only listener). A controllable oneshot trigger would
// exercise a DIFFERENT future than the one production uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_drives_graceful_shutdown_sessions_end_and_clean_exit() {
    let _socket_guard = REAL_SOCKET_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = test_state(tmp.path());
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");

    let id_a = state
        .create_session(&ws, None, false)
        .await
        .expect("create a");
    let id_b = state
        .create_session(&ws, None, false)
        .await
        .expect("create b");

    // The TEST subscribes before shutdown so session.end is observable.
    let mut subs = Vec::new();
    for id in [id_a, id_b] {
        let sub = state
            .subscribe(id, None)
            .await
            .expect("subscribe before shutdown");
        subs.push((id, sub));
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("local addr");

    // The REAL graceful-shutdown path, against OUR state.
    let server_task = tokio::spawn(navai::server::routes::serve_state_on(
        state.clone(),
        listener,
    ));

    // Wait until the socket answers, then SIGTERM this process.
    common::server::wait_until_healthy(addr).await;

    // SIGTERM: the same signal the production shutdown_signal handler
    // selects on (SignalKind::terminate).
    navai::server::routes::send_test_sigterm();

    // (ii) serve RETURNS CLEANLY.
    let serve_result = tokio::time::timeout(std::time::Duration::from_secs(15), server_task)
        .await
        .expect("server task finished within timeout")
        .expect("server task join");
    assert!(
        serve_result.is_ok(),
        "serve returned cleanly: {serve_result:?}"
    );

    // (i) each session emitted session.end as its LAST event, gapless.
    for (id, sub) in subs {
        let mut events = Vec::new();
        if let navai::bus::ResumeOutcome::Replay(replay) = sub.replay {
            events.extend(replay);
        }
        if let Some(mut live) = sub.live {
            while let Some(env) =
                tokio::time::timeout(std::time::Duration::from_secs(5), live.recv())
                    .await
                    .expect("drain within timeout")
            {
                events.push(env);
            }
        }
        let last = events
            .last()
            .unwrap_or_else(|| panic!("session {id}: events"));
        assert_eq!(last.kind, "session.end", "session {id} terminal event");
        assert_eq!(last.payload["final_seq"], last.seq, "session {id}");
        assert!(
            events.windows(2).all(|w| w[1].seq == w[0].seq + 1),
            "gapless terminal stream for session {id}"
        );
    }

    // Every session left the registry (routing released, GET still answers
    // from the store).
    assert_eq!(state.session_count(), 0);
    for id in [id_a, id_b] {
        let summary = state.session_summary(id).await.expect("stored");
        assert_eq!(summary.actor_state, "stopped");
        assert_not_routable(&state, id).await;
    }
}

/// A command to a closed session is refused: the store answers reads, never
/// commands.
async fn assert_not_routable(state: &AppState, id: navai::bus::command::SessionId) {
    let command = text_command(id, "anyone there?");
    assert!(
        matches!(
            state.send(command).await,
            Err(navai::error::ServerError::ResumeRequired)
        ),
        "closed session is not routable"
    );
}

// Given an alias with no context_window whose provider cannot list models
// When POST /v1/sessions
// Then 422 {reason:"no_context_window"} with the exact text naming the
//      resolved models.json; no session is created
#[tokio::test]
async fn create_session_without_a_known_window_is_422_no_context_window() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let cfg = TestServerConfig::unreachable(tmp.path()).without_window();
    let state = cfg.state();
    let (status, body) = request_json(
        &app(state),
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": ws.to_str().expect("utf8 ws") })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["reason"], "no_context_window");
    assert_eq!(
        body["error"],
        format!(
            "Chat alias 'default' (provider 'mock', model 'mock-model') has no known context window: the provider's model list did not report one. Set \"context_window\" for it in {}, or make the provider reachable and try again.",
            cfg.models_path().display()
        )
    );
}
