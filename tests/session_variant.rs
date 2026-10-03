//! The session's model variant: chosen at creation (or the alias default),
//! saved with the session, checked on resume, changed with `/variant` at the
//! next turn boundary, and merged into the chat request body.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use navai::error::ServerError;
use navai::server::{app, AppState};
use navai::session::VariantChoice;

mod common;
use common::server::{collect_until, request_json, send_text, wait_for, TestServerConfig};
use common::sse::{http_sse, spawn_capturing_provider, spawn_gated_provider, sse_body};

fn offer() -> Value {
    json!({
        "low": {"reasoning_effort": "low"},
        "high": {"reasoning_effort": "high"}
    })
}

fn tmp_ws() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    (tmp, ws)
}

fn config(tmp: &std::path::Path, url: &str, default: Option<&str>) -> TestServerConfig {
    TestServerConfig::mock(tmp, url).variants(offer(), default)
}

async fn saved(state: &AppState, id: ulid::Ulid) -> Option<String> {
    state.history().meta(id).await.unwrap().unwrap().variant
}

fn done_body() -> Vec<u8> {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    http_sse(&sse_body(&[done]))
}

// Given an alias with variants and a default
// When sessions are created without a variant, with one, with `none`, and with an unknown one
// Then the default, the named one, none and an error are the outcomes, each saved
#[tokio::test]
async fn creation_picks_default_named_none_or_fails() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("high")).state();

    let id = state.create_session(&ws, None, None).await.unwrap();
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"));
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("high")
    );

    let id = state
        .create_session_with_variant(&ws, None, None, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"));

    let id = state
        .create_session_with_variant(&ws, None, None, VariantChoice::from_word(Some("off")))
        .await
        .unwrap();
    assert_eq!(saved(&state, id).await, None);
    assert_eq!(state.session_summary(id).await.unwrap().variant, None);

    let before = state.session_count();
    let err = state
        .create_session_with_variant(&ws, None, None, VariantChoice::from_word(Some("none")))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::UnknownVariant { .. }), "{err:?}");
    let err = state
        .create_session_with_variant(&ws, None, None, VariantChoice::from_word(Some("max")))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::UnknownVariant { .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("'default'") && text.contains("'max'") && text.contains("high, low"));
    assert_eq!(
        state.session_count(),
        before,
        "no session for a bad variant"
    );
}

// Given an alias without a default
// When a session is created
// Then it has no variant
#[tokio::test]
async fn an_alias_without_default_starts_unvaried() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", None).state();
    let id = state.create_session(&ws, None, None).await.unwrap();
    assert_eq!(saved(&state, id).await, None);
}

// Given a saved variant
// When the session is resumed, with the variant still on offer or taken away
// Then it comes back with the variant, or the resume is an explicit error
#[tokio::test]
async fn resume_keeps_the_variant_and_a_missing_one_is_an_error() {
    let (tmp, ws) = tmp_ws();
    let cfg = config(tmp.path(), "http://127.0.0.1:1/v1", None);
    let state = cfg.state();
    let id = state
        .create_session_with_variant(&ws, None, None, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    state
        .create_or_resume_session(&ws, None, None, Some(id))
        .await
        .expect("resumes");
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("low")
    );
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    // The alias stops offering "low".
    TestServerConfig::mock(tmp.path(), "http://127.0.0.1:1/v1")
        .variants(json!({"high": {"reasoning_effort": "high"}}), None)
        .config();
    let err = state
        .create_or_resume_session(&ws, None, None, Some(id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServerError::VariantUnavailable { .. }),
        "{err:?}"
    );
    let text = err.to_string();
    for part in ["'default'", "'low'", "models.json", &id.to_string()] {
        assert!(text.contains(part), "{part} in {text}");
    }
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"), "kept");
}

// Given a session with a variant
// When /variant runs bare, with a name, with `off`, and with an unknown name
// Then it lists, switches (saved before it answers), clears, and refuses
#[tokio::test]
async fn variant_command_lists_sets_clears_and_refuses() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("high")).state();
    let id = state.create_session(&ws, None, None).await.unwrap();

    let out = state.run_command(id, "variant", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    let data = out.data.unwrap();
    assert_eq!(data["variant"], "high");
    assert_eq!(data["offered"], json!(["high", "low"]));

    let out = state.run_command(id, "variant", "low").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.as_ref().unwrap()["variant"], "low");
    assert_eq!(out.data.as_ref().unwrap()["applies"], "now");
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"));
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("low")
    );

    let out = state.run_command(id, "variant", "off").await.unwrap();
    assert!(out.ok);
    assert_eq!(
        out.render_text(),
        "variant: (none set); on offer: high, low"
    );
    assert_eq!(out.data.unwrap()["variant"], Value::Null);
    assert_eq!(saved(&state, id).await, None);

    // `none` is an ordinary name here: not offered, so refused, not a clear.
    state.run_command(id, "variant", "low").await.unwrap();
    let out = state.run_command(id, "variant", "none").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("'none'"));
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"), "kept");

    let out = state.run_command(id, "variant", "max").await.unwrap();
    assert!(!out.ok);
    let error = out.error.unwrap();
    assert!(
        error.contains("'max'") && error.contains("high, low"),
        "{error}"
    );
    assert_eq!(
        saved(&state, id).await.as_deref(),
        Some("low"),
        "a refusal saves nothing"
    );

    let out = state.run_command(id, "variant", "low high").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("usage"));
    assert!(out.name == "variant");
    assert!(!state
        .run_command(id, "context", "")
        .await
        .unwrap()
        .error
        .unwrap()
        .is_empty());
}

// Given the HTTP surface
// When a session is created with a variant, and read back
// Then GET shows it; null clears, omitted is the default; an unknown name
//      (`none` included) is a 422; a resume with one is a 400
#[tokio::test]
async fn http_create_and_get_carry_the_variant() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("high")).state();
    let router = app(state.clone());
    let ws_str = ws.display().to_string();

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str, "variant": "low"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["session_id"].as_str().unwrap().to_string();
    let (_, got) = request_json(&router, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(got["variant"], "low");

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = body["session_id"].as_str().unwrap().to_string();
    let (_, got) = request_json(&router, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(got["variant"], "high");

    // An explicit null clears the variant; a string is always an exact name.
    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str, "variant": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = body["session_id"].as_str().unwrap().to_string();
    let (_, got) = request_json(&router, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(got["variant"], Value::Null);
    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str, "variant": "none"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["reason"], "unknown_variant");

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str, "variant": "max"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["reason"], "unknown_variant");

    let (status, _) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws_str, "variant": "low", "resume": id})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Given a turn that is running on "high"
// When /variant low arrives
// Then it is saved and reported, the running request is unchanged, and the
//      next request carries the new value
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_during_a_turn_applies_at_the_next_turn() {
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![done_body(), done_body()], Arc::clone(&release)).await;
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), &provider.url, Some("high")).state();
    let id = state.create_session(&ws, None, None).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "first").await;
    wait_for("the first request", || provider.requests().len() == 1).await;

    let out = state.run_command(id, "variant", "low").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.unwrap()["applies"], "next_turn");
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"));
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("low")
    );

    release.notify_one();
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;

    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "second").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;

    let bodies: Vec<Value> = provider
        .bodies()
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[0]["reasoning_effort"], "high",
        "running turn kept it"
    );
    assert_eq!(
        bodies[1]["reasoning_effort"], "low",
        "next turn has the switch"
    );
}

// Given a session cleared with /variant none
// When it takes a turn
// Then the request carries no variant key
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cleared_variant_sends_no_override() {
    let provider = spawn_capturing_provider(vec![done_body()]).await;
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), &provider.url, Some("high")).state();
    let id = state.create_session(&ws, None, None).await.unwrap();
    assert!(state.run_command(id, "variant", "off").await.unwrap().ok);
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    let body: Value = serde_json::from_str(&provider.bodies()[0]).unwrap();
    assert!(body.get("reasoning_effort").is_none(), "{body}");
}

// Given `navai run --variant high`
// Then the request body carries the variant; an unknown one fails the run
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_variant_reaches_the_wire() {
    let provider = spawn_capturing_provider(vec![done_body()]).await;
    let (tmp, ws) = tmp_ws();
    let cfg = config(tmp.path(), &provider.url, None);
    cfg.config();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let run = move |variant: &'static str, ws: std::path::PathBuf| {
        let (root, models) = (root.clone(), models.clone());
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_navai"))
                .args(["run", "--workspace"])
                .arg(&ws)
                .arg("--models")
                .arg(&models)
                .args(["--variant", variant, "--prompt", "go"])
                .env("NAVAI_TEST_KEY", "test-key")
                .env("HOME", root.join("home"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .output()
                .expect("run the binary")
        })
    };
    let out = run("high", ws.clone()).await.unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body: Value = serde_json::from_str(&provider.bodies()[0]).unwrap();
    assert_eq!(body["reasoning_effort"], "high");

    let out = run("max", ws).await.unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no variant 'max'"));
}
