//! Explicit archive markers preserve legacy history without preparing a runtime.
use axum::http::StatusCode;
use http_body_util::BodyExt;
use navai::bus::event::{EventEnvelope, EventKind};
use navai::error::ServerError;
use navai::provider::openai::ChatMessage;
use navai::server::routes::app as router;
use navai::session::store::HistoryStore;
use serde_json::json;
use tower::ServiceExt;
use ulid::Ulid;

mod common;
use common::server::{driver_command, request_json, TestServerConfig};

const REASON: &str = "Historic model identity has no proven resolution";

#[tokio::test]
async fn service_marking_requires_offline_and_raw_live_marker_refuses_all_admissions() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    let id = state.create_session(&ws, None, None).await.unwrap();
    let store = state.history();
    store.flush().await.unwrap();
    let before = store.load(id).await.unwrap().unwrap();
    assert!(state.mark_archive(id, REASON).await.is_err());
    let after = store.load(id).await.unwrap().unwrap();
    assert!(after.meta.resume_disabled_reason.is_none());
    assert_eq!(after.meta.last_seq, before.meta.last_seq);
    assert_eq!(
        serde_json::to_value(after.envelopes).unwrap(),
        serde_json::to_value(&before.envelopes).unwrap()
    );

    // Deliberately violate the raw primitive's offline contract to exercise the
    // defensive boundary against a marker present alongside a retained runtime.
    store.mark_archive(id, REASON).await.unwrap();
    assert!(matches!(
        state.send(driver_command(id, "must not run".into())).await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state.run_command(id, "model", "default").await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state.run_command(id, "unknown-archive-command", "").await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state
            .answer_permission(
                id,
                Ulid::new(),
                navai::agent::permissions::PermissionDecision::AllowOnce
            )
            .await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state.continue_child(id, id).await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state.steer_child(id, id, "must not steer", true).await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state.close_session(id).await,
        Err(ServerError::SessionArchived { .. })
    ));
    assert!(matches!(
        state
            .create_or_resume_session(&ws, None, None, Some(id))
            .await,
        Err(ServerError::SessionArchived { .. })
    ));
    let app = router(state.clone());
    for (path, body) in [
        (
            format!("/v1/sessions/{id}/messages"),
            json!({"target_actor_id":"agent:main","content":[{"type":"text","text":"no admission"}]}),
        ),
        (
            format!("/v1/sessions/{id}/commands"),
            json!({"name":"unknown-archive-command"}),
        ),
        (
            format!("/v1/sessions/{id}/permissions/{}", Ulid::new()),
            json!({"decision":"allow_once"}),
        ),
        (
            format!("/v1/sessions/{id}/children/{id}/continue"),
            json!({}),
        ),
        (format!("/v1/sessions/{id}/stop"), json!({})),
    ] {
        let (status, body) = request_json(&app, "POST", &path, Some(&body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body["reason"], "session_archived");
    }
    let (status, meta) = request_json(&app, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["read_only"], true);
    assert_eq!(meta["promptable"], false);
    assert!(
        state.subscribe(id, None).await.is_ok(),
        "event reads remain allowed"
    );
    store.flush().await.unwrap();
    let after = store.load(id).await.unwrap().unwrap();
    assert_eq!(after.meta.last_seq, before.meta.last_seq);
    assert_eq!(after.meta.updated_at, before.meta.updated_at);
    assert_eq!(after.meta.model_resolution, before.meta.model_resolution);
    assert_eq!(
        serde_json::to_value(after.envelopes).unwrap(),
        serde_json::to_value(before.envelopes).unwrap()
    );
    assert_eq!(
        serde_json::to_value(after.transcript).unwrap(),
        serde_json::to_value(before.transcript).unwrap()
    );
    // Explicit deletion remains allowed, including retained-runtime teardown.
    state.delete(id).await.unwrap();
    assert!(!store.exists(id).await.unwrap());

    let stopped = state.create_session(&ws, None, None).await.unwrap();
    state.close_session(stopped).await.unwrap();
    state.mark_archive(stopped, REASON).await.unwrap();
    assert!(store.meta(stopped).await.unwrap().unwrap().read_only());
    assert!(matches!(
        state.close_session(stopped).await,
        Err(ServerError::SessionArchived { .. })
    ));
}

#[tokio::test]
async fn old_schema_adds_nullable_marker_without_classifying_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("old.db");
    let id = Ulid::new();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, workspace TEXT NOT NULL, model TEXT NOT NULL, profile TEXT,
            title TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
            last_seq INTEGER NOT NULL DEFAULT -1, transcript TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id,workspace,model,created_at,updated_at)
            VALUES (?1,'/workspace','original-key','old-created','old-updated')",
            [id.to_string()],
        )
        .unwrap();
    }
    let store = HistoryStore::open(&db).unwrap();
    let meta = store.meta(id).await.unwrap().unwrap();
    assert!(!meta.read_only());
    assert!(meta.resume_disabled_reason.is_none());
    assert!(meta.model_resolution.is_none());
    assert_eq!(meta.model, "original-key");
    store.mark_archive(id, REASON).await.unwrap();
    drop(store);
    let reopened = HistoryStore::open(&db).unwrap();
    let meta = reopened.meta(id).await.unwrap().unwrap();
    assert!(meta.read_only());
    assert_eq!(meta.resume_disabled_reason.as_deref(), Some(REASON));
    assert_eq!(meta.created_at, "old-created");
    assert_eq!(meta.updated_at, "old-updated");
}

#[tokio::test]
async fn archive_marker_is_explicit_validated_and_preserves_store_history() {
    let tmp = tempfile::tempdir().unwrap();
    let store = HistoryStore::open(tmp.path().join("history.db")).unwrap();
    let id = Ulid::new();
    let normal = Ulid::new();
    for session in [id, normal] {
        store
            .record_session(session, "/workspace", "old-original-key", None)
            .await
            .unwrap();
    }
    let transcript = vec![ChatMessage::User {
        content: "original transcript".into(),
        provenance: Default::default(),
    }];
    store.save_transcript(id, transcript.clone());
    let event = EventEnvelope::new(
        id,
        0,
        EventKind::MessageReceived,
        json!({"content":"original event", "source":"user:http"}),
    );
    store.append_event(id, event.clone());
    store.flush().await.unwrap();
    let before = store.load(id).await.unwrap().unwrap();
    for reason in [
        "",
        "  ",
        "line\nbreak",
        "\u{0}",
        "\u{1b}[31m",
        "\u{202e}spoof",
        "\u{2066}isolate",
        "\u{061c}arabic",
        "\u{200e}left",
        "\u{200f}right",
        &"x".repeat(513),
    ] {
        assert!(store.mark_archive(id, reason).await.is_err());
        assert!(store
            .meta(id)
            .await
            .unwrap()
            .unwrap()
            .resume_disabled_reason
            .is_none());
    }
    let unknown = Ulid::new();
    assert!(store.mark_archive(unknown, REASON).await.is_err());
    assert!(!store.exists(unknown).await.unwrap());
    store.mark_archive(id, REASON).await.unwrap();
    let after = store.load(id).await.unwrap().unwrap();
    assert_eq!(after.meta.resume_disabled_reason.as_deref(), Some(REASON));
    assert_eq!(after.meta.updated_at, before.meta.updated_at);
    assert_eq!(after.meta.last_seq, before.meta.last_seq);
    assert_eq!(after.meta.model, "old-original-key");
    assert!(after.meta.model_resolution.is_none());
    assert_eq!(
        serde_json::to_value(after.transcript).unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
    assert_eq!(
        serde_json::to_value(after.envelopes).unwrap(),
        json!([event])
    );
    let rows = store.list("/workspace").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|row| row.meta.id == id)
            .unwrap()
            .meta
            .resume_disabled_reason
            .as_deref(),
        Some(REASON)
    );
    assert!(rows
        .iter()
        .find(|row| row.meta.id == normal)
        .unwrap()
        .meta
        .resume_disabled_reason
        .is_none());
    store.delete(id).await.unwrap();
    assert!(!store.exists(id).await.unwrap());
    assert!(store.exists(normal).await.unwrap());
}

#[tokio::test]
async fn archived_legacy_http_reads_work_and_resume_and_mutations_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::unreachable(tmp.path());
    let state = config.state();
    let store = state.history();
    let id = Ulid::new();
    let normal = Ulid::new();
    let workspace = ws.display().to_string();
    for session in [id, normal] {
        store
            .record_session(session, &workspace, "historic-original-key", None)
            .await
            .unwrap();
    }
    store.save_transcript(
        id,
        vec![ChatMessage::User {
            content: "historic transcript".into(),
            provenance: Default::default(),
        }],
    );
    let event = EventEnvelope::new(
        id,
        0,
        EventKind::MessageReceived,
        json!({"content":"historic event", "source":"user:http"}),
    );
    store.append_event(id, event.clone());
    state.mark_archive(id, REASON).await.unwrap();
    let before = store.load(id).await.unwrap().unwrap();
    // Neither catalog nor auth can be loaded: the refusal must precede preparation.
    std::fs::remove_file(config.models_path()).unwrap();
    std::fs::remove_file(tmp.path().join("cfg/auth.json")).unwrap();
    let app = router(state.clone());
    let (status, meta) = request_json(&app, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["model"], "historic-original-key");
    assert_eq!(meta["read_only"], true);
    assert_eq!(meta["resume_disabled_reason"], REASON);
    assert_eq!(meta["promptable"], false);
    assert!(meta.get("parent_session_id").is_none());
    let (status, list) = request_json(
        &app,
        "GET",
        &format!("/v1/sessions?workspace={workspace}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = list["sessions"].as_array().unwrap();
    let archived = rows.iter().find(|row| row["id"] == id.to_string()).unwrap();
    assert_eq!(archived["read_only"], true);
    assert_eq!(archived["resume_disabled_reason"], REASON);
    assert_eq!(archived["promptable"], false);
    assert_eq!(archived["live"], false);
    let unaffected = rows
        .iter()
        .find(|row| row["id"] == normal.to_string())
        .unwrap();
    assert_eq!(unaffected["read_only"], false);
    assert!(unaffected["resume_disabled_reason"].is_null());
    let (status, refusal) = request_json(
        &app,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace":workspace,"resume":id.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refusal["reason"], "session_archived");
    assert_eq!(refusal["resume_disabled_reason"], REASON);
    for (path, body) in [
        (
            format!("/v1/sessions/{id}/messages"),
            json!({"target_actor_id":"agent:main","content":[{"type":"text","text":"do not run"}]}),
        ),
        (
            format!("/v1/sessions/{id}/commands"),
            json!({"name":"model","arguments":"default"}),
        ),
        (
            format!("/v1/sessions/{id}/permissions/{}", Ulid::new()),
            json!({"decision":"allow_once"}),
        ),
        (format!("/v1/sessions/{id}/stop"), json!({})),
        (
            format!("/v1/sessions/{id}/children/{}/continue", Ulid::new()),
            json!({}),
        ),
    ] {
        let (status, _) = request_json(&app, "POST", &path, Some(&body)).await;
        assert!(!status.is_success(), "archive mutation accepted: {path}");
    }
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/v1/sessions/{id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let sse = String::from_utf8(body.to_vec()).unwrap();
    assert!(sse.contains("historic event"));
    let after = store.load(id).await.unwrap().unwrap();
    assert_eq!(after.meta.model, before.meta.model);
    assert_eq!(after.meta.model_resolution, before.meta.model_resolution);
    assert_eq!(after.meta.last_seq, before.meta.last_seq);
    assert_eq!(after.meta.updated_at, before.meta.updated_at);
    assert_eq!(
        serde_json::to_value(after.transcript).unwrap(),
        serde_json::to_value(before.transcript).unwrap()
    );
    assert_eq!(
        serde_json::to_value(after.envelopes).unwrap(),
        json!([event])
    );
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "stopped"
    );
    assert_eq!(
        state.session_count(),
        0,
        "archive refusal creates no runtime"
    );
    state.delete(id).await.unwrap();
    assert!(!store.exists(id).await.unwrap());
}
