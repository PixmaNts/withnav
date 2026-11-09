//! Integration tests: the attachment-upload route follows the conversation's
//! state. An active or stopped conversation accepts an upload; an archived one
//! is refused with the same `session_archived` error the message route uses and
//! stores nothing; an unknown or deleted id is 404.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use withnav::server::app;

mod common;
use common::server::{TestServerConfig, request_json};

/// A valid 1×1 RGBA PNG that satisfies full raster decoding.
fn png(_width: u32, _height: u32) -> Vec<u8> {
    [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x10,
        0x54, 0x32, 0xfe, 0x0f, 0x00, 0x02, 0x14, 0x01, 0x66, 0x04, 0xe0, 0xde, 0x7f, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
    .to_vec()
}

/// POSTs `body` to `uri` and returns the status and the raw response bytes.
async fn post_response(
    app: &axum::Router,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", content_type)
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("infallible");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, body)
}

/// POSTs raw image bytes to the upload route and returns the status and the
/// parsed JSON body (the uploaded `ImageRef`, or the shared `{error, reason}`).
async fn upload(app: &axum::Router, id: &str, name: &str, bytes: Vec<u8>) -> (StatusCode, Value) {
    let uri = format!("/v1/sessions/{id}/attachments?name={name}");
    let (status, body) = post_response(app, &uri, "application/octet-stream", bytes).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// How many attachment rows the history database holds for `session`.
fn attachment_count(db: &std::path::Path, session: &str) -> i64 {
    let connection = rusqlite::Connection::open(db).expect("open history db");
    connection
        .query_row(
            "SELECT COUNT(*) FROM attachments WHERE session_id = ?1",
            [session],
            |row| row.get(0),
        )
        .expect("attachment count")
}

// Given active, stopped, archived and deleted conversations
// When an image is uploaded to each
// Then only active and stopped are accepted, archived is refused with the
//      message route's error and stores nothing, and deleted is 404
#[tokio::test]
async fn uploads_follow_the_conversation_state() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let config = TestServerConfig::unreachable(tmp.path());
    let state = config.state();
    let router = app(state.clone());
    let workspace = ws.display().to_string();

    // Active: a live conversation accepts an upload.
    let (status, created) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": workspace})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["session_id"].as_str().unwrap().to_string();
    let parsed: ulid::Ulid = id.parse().unwrap();
    let (status, image) = upload(&router, &id, "shot.png", png(7, 9)).await;
    assert_eq!(status, StatusCode::OK, "{image}");
    assert_eq!(image["media_type"], "image/png");

    // Stopped: the conversation has no runtime, yet the upload is accepted; a
    // resume keeps the id, and a message carrying the stored image is admitted.
    state.close_session(parsed).await.unwrap();
    assert!(!state.session_has_runtime(parsed), "runtime is closed");
    let (status, image) = upload(&router, &id, "resume.png", png(5, 3)).await;
    assert_eq!(status, StatusCode::OK, "{image}");
    let attachment = image["attachment_id"].as_str().unwrap().to_string();

    let (status, resumed) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": workspace, "resume": id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["session_id"], id, "resume keeps the id");
    let (status, accepted) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/messages"),
        Some(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [
                {"type": "image", "attachment_id": attachment},
                {"type": "text", "text": "look"}
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    // The 200 above is the stored attachment resolving, not the route ignoring
    // the block: an id the conversation does not hold is refused.
    let (status, refused) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/messages"),
        Some(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [
                {"type": "image", "attachment_id": "not-a-real-attachment"},
                {"type": "text", "text": "look"}
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    state.close_session(parsed).await.unwrap();

    // Archived: refused with the message route's error and nothing stored.
    let (status, created) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": workspace})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let archived = created["session_id"].as_str().unwrap().to_string();
    let archived_id: ulid::Ulid = archived.parse().unwrap();
    state.close_session(archived_id).await.unwrap();
    state
        .mark_archive(
            archived_id,
            "historic model identity has no proven resolution",
        )
        .await
        .unwrap();
    let (status, upload_body) = post_response(
        &router,
        &format!("/v1/sessions/{archived}/attachments?name=nope.png"),
        "application/octet-stream",
        png(2, 2),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{upload_body:?}");
    let refusal: Value = serde_json::from_slice(&upload_body).expect("json refusal");
    assert_eq!(refusal["reason"], "session_archived");
    assert_eq!(
        refusal["resume_disabled_reason"],
        "historic model identity has no proven resolution"
    );
    // The message route refuses the same archived conversation with the same
    // body: one archived contract, wherever the bytes would come from.
    let (message_status, message_body) = post_response(
        &router,
        &format!("/v1/sessions/{archived}/messages"),
        "application/json",
        json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{"type": "text", "text": "no admission"}]
        })
        .to_string()
        .into_bytes(),
    )
    .await;
    assert_eq!(message_status, StatusCode::FORBIDDEN);
    assert_eq!(upload_body, message_body, "one archived refusal body");
    assert_eq!(
        attachment_count(&tmp.path().join("history.db"), &archived),
        0,
        "an archived conversation stores nothing"
    );

    // Deleted: 404, nothing stored.
    let (status, created) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": workspace})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let deleted = created["session_id"].as_str().unwrap().to_string();
    let deleted_id: ulid::Ulid = deleted.parse().unwrap();
    state.delete(deleted_id).await.unwrap();
    let (status, refusal) = upload(&router, &deleted, "gone.png", png(1, 1)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refusal}");
    assert_eq!(refusal["reason"], "unknown_session");

    state.close_all().await;
}
