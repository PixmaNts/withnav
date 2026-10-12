//! The session's model variant: chosen at creation (or the alias default),
//! saved with the session, checked on resume, changed with `/variant` at the
//! next turn boundary, and merged into the chat request body.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::error::ServerError;
use withnav::server::{app, AppState};
use withnav::session::VariantChoice;

mod common;
use common::server::{
    collect_until, request_json, send_text, temp_workspace as tmp_ws, wait_for, TestServerConfig,
};
use common::sse::{http_sse, spawn_capturing_provider, spawn_gated_provider, sse_body};

fn offer() -> Value {
    json!({
        "low": {"reasoning_effort": "low"},
        "high": {"reasoning_effort": "high"}
    })
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

#[tokio::test]
async fn child_inherits_parent_variant_and_saved_resolution() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("high")).state();
    let parent = state
        .create_session_with_variant(
            &ws,
            Some("mock/mock-model"),
            false,
            VariantChoice::Exact("low".into()),
        )
        .await
        .unwrap();
    let before = state.history().meta(parent).await.unwrap().unwrap();
    let path = tmp.path().join("models.json");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["models"]["mock/mock-model"]["context_window"] = json!(256_000);
    models["models"]["mock/mock-model"]["variants"]["low"] = json!({"reasoning_effort":"high"});
    std::fs::write(path, models.to_string()).unwrap();
    let child = state
        .create_child_session(parent, "vigie", "inspect")
        .await
        .unwrap();
    let after = state.history().meta(child).await.unwrap().unwrap();
    assert_eq!(after.model, before.model);
    assert_eq!(after.variant.as_deref(), Some("low"));
    assert_eq!(after.variant_override, before.variant_override);
    assert_eq!(after.model_resolution, before.model_resolution);
    state.close_all().await;
}

// Given an alias with variants and a default
// When sessions are created without a variant, with one, with `none`, and with an unknown one
// Then the default, the named one, none and an error are the outcomes, each saved
#[tokio::test]
async fn creation_picks_default_named_none_or_fails() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("high")).state();

    let id = state.create_session(&ws, None, false).await.unwrap();
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"));
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("high")
    );

    let id = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    assert_eq!(saved(&state, id).await.as_deref(), Some("low"));

    let id = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("off")))
        .await
        .unwrap();
    assert_eq!(saved(&state, id).await, None);
    assert_eq!(state.session_summary(id).await.unwrap().variant, None);

    let before = state.session_count();
    let err = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("none")))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::UnknownVariant { .. }), "{err:?}");
    let err = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("max")))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::UnknownVariant { .. }), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains("'mock/mock-model'") && text.contains("'max'") && text.contains("high, low")
    );
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
    let id = state.create_session(&ws, None, false).await.unwrap();
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
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    state
        .create_or_resume_session(&ws, None, false, Some(id))
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
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServerError::VariantUnavailable { .. }),
        "{err:?}"
    );
    let text = err.to_string();
    for part in ["'mock/mock-model'", "'low'", "models.json", &id.to_string()] {
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
    let id = state.create_session(&ws, None, false).await.unwrap();

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
        .run_command(id, "compact", "")
        .await
        .unwrap()
        .error
        .unwrap()
        .is_empty());
}

// Given a session with a default variant
// When /variant changes it, then /context runs
// Then the gauge shows the selected variant, and context.updated carried it
#[tokio::test]
async fn context_shows_the_selected_variant() {
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), "http://127.0.0.1:1/v1", Some("low")).state();
    let id = state.create_session(&ws, None, false).await.unwrap();
    let context = |state: AppState| async move {
        state
            .run_command(id, "context", "")
            .await
            .unwrap()
            .data
            .unwrap()
    };
    assert_eq!(context(state.clone()).await["variant"], "low");

    let sub = state.subscribe(id, None).await.unwrap();
    let out = state.run_command(id, "variant", "high").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(context(state.clone()).await["variant"], "high");
    assert_eq!(
        state.context(id).await.unwrap().variant.as_deref(),
        Some("high")
    );
    let events = collect_until(sub, |e| e.kind == "context.updated").await;
    let updated = events.iter().find(|e| e.kind == "context.updated").unwrap();
    assert_eq!(updated.payload["variant"], "high");

    let out = state.run_command(id, "variant", "off").await.unwrap();
    assert!(out.ok);
    assert_eq!(context(state.clone()).await["variant"], Value::Null);
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
async fn variant_changes_are_refused_during_a_turn_and_apply_when_idle() {
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = spawn_gated_provider(vec![done_body(), done_body()], Arc::clone(&release)).await;
    let (tmp, ws) = tmp_ws();
    let state = config(tmp.path(), &provider.url, Some("high")).state();
    let id = state.create_session(&ws, None, false).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    send_text(&state, id, "first").await;
    wait_for("the first request", || provider.requests().len() == 1).await;

    let out = state.run_command(id, "variant", "low").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"));
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("high")
    );

    release.notify_one();
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;

    assert!(state.run_command(id, "variant", "low").await.unwrap().ok);
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
    let id = state.create_session(&ws, None, false).await.unwrap();
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

// Given `nav run --variant high` and `--variant off`
// Then the request body carries the variant, or none; an unknown one fails the run
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_variant_reaches_the_wire() {
    let provider = spawn_capturing_provider(vec![done_body(), done_body()]).await;
    let (tmp, ws) = tmp_ws();
    let cfg = config(tmp.path(), &provider.url, None);
    cfg.config();
    std::fs::create_dir_all(tmp.path().join("config/withnav")).unwrap();
    std::fs::write(
        tmp.path().join("config/withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let run = move |variant: &'static str, ws: std::path::PathBuf| {
        let (root, models) = (root.clone(), models.clone());
        tokio::task::spawn_blocking(move || {
            common::nav_command(&root)
                .args(["run", "--workspace"])
                .arg(&ws)
                .arg("--models")
                .arg(&models)
                .args(["--variant", variant, "--prompt", "go"])
                .env("WITHNAV_TEST_KEY", "test-key")
                .output()
                .expect("run the binary")
        })
    };
    let out = run("high", ws.clone()).await.unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body: Value = serde_json::from_str(&provider.bodies()[0]).unwrap();
    assert_eq!(body["reasoning_effort"], "high");

    let out = run("off", ws.clone()).await.unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body: Value = serde_json::from_str(&provider.bodies()[1]).unwrap();
    assert!(body.get("reasoning_effort").is_none(), "{body}");

    let out = run("max", ws).await.unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no variant 'max'"));
}

fn tiers_body(tiers: &[&str]) -> Value {
    json!({"data": [{
        "id": "mock-model", "context_length": 128000,
        "capabilities": {"effort_tiers": tiers}
    }]})
}

async fn serve_tiers(server: &wiremock::MockServer, tiers: &[&str]) {
    mount_models_response(
        server,
        wiremock::ResponseTemplate::new(200).set_body_json(tiers_body(tiers)),
    )
    .await;
}

async fn serve_down(server: &wiremock::MockServer) {
    mount_models_response(server, wiremock::ResponseTemplate::new(503)).await;
}

async fn mount_models_response(
    server: &wiremock::MockServer,
    response: wiremock::ResponseTemplate,
) {
    server.reset().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Makes the cached model list look `days` days old.
fn age_cache(tmp: &std::path::Path, days: u64) {
    let dir = tmp.join("data").join("model-catalog");
    for entry in std::fs::read_dir(dir).expect("cache dir") {
        let path = entry.unwrap().path();
        let mut cache: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let old = cache["fetched_at"].as_u64().unwrap() - days * 86_400;
        cache["fetched_at"] = json!(old);
        std::fs::write(&path, cache.to_string()).unwrap();
    }
}

/// A session on a generated variant ("high"), stopped and saved.
async fn stopped_generated(
    server: &wiremock::MockServer,
    tmp: &std::path::Path,
    ws: &std::path::Path,
) -> (AppState, ulid::Ulid) {
    serve_tiers(server, &["low", "high"]).await;
    let state = TestServerConfig::mock(tmp, &format!("{}/v1", server.uri()))
        .omniroute()
        .state();
    let id = state
        .create_session_with_variant(ws, None, false, VariantChoice::from_word(Some("high")))
        .await
        .expect("create");
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();
    (state, id)
}

// Given a session saved on a generated variant, and a model list that is not
//      available (nothing cached, provider down)
// When the session is resumed
// Then it resumes with the saved variant: nothing says it is gone
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_with_a_cold_cache_and_the_provider_down_keeps_the_variant() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    let (state, id) = stopped_generated(&server, tmp.path(), &ws).await;
    std::fs::remove_dir_all(tmp.path().join("data/model-catalog")).expect("cold cache");
    serve_down(&server).await;

    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resumes offline");
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("high")
    );
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"));
}

// Given the same, but only an old cached list whose refresh fails
// When the session is resumed
// Then the saved variant is accepted, whether the old list names it or not
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_with_a_stale_list_and_the_provider_down_keeps_the_variant() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    let (state, id) = stopped_generated(&server, tmp.path(), &ws).await;
    age_cache(tmp.path(), 3);
    serve_down(&server).await;
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("stale list names it");
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    // An old list that does not name it proves nothing either.
    std::fs::remove_dir_all(tmp.path().join("data/model-catalog")).unwrap();
    // Seed an old list with only "low", then take the provider down.
    serve_tiers(&server, &["low"]).await;
    let seeded = TestServerConfig::mock(tmp.path(), &format!("{}/v1", server.uri()))
        .omniroute()
        .state();
    let other = seeded
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    seeded.close_session(other).await.unwrap();
    age_cache(tmp.path(), 3);
    serve_down(&server).await;
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("an old list without it does not remove it");
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("high")
    );
}

// Given the provider now reports tiers without the saved one
// When the session is resumed
// Then it is an explicit 409-class error naming the alias and variant
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_after_the_provider_removed_the_tier_is_an_error() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    let (state, id) = stopped_generated(&server, tmp.path(), &ws).await;
    age_cache(tmp.path(), 3);
    serve_tiers(&server, &["low"]).await;

    let err = state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServerError::VariantUnavailable { .. }),
        "{err:?}"
    );
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"), "kept");
}

// Given a generated offer that cannot be confirmed (provider down, nothing
//      cached)
// When a new session asks for a named variant
// Then it is a retryable error, not an unknown-variant verdict
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn choosing_a_variant_while_the_offer_is_unknown_is_retryable() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    serve_down(&server).await;
    let state = TestServerConfig::mock(tmp.path(), &format!("{}/v1", server.uri()))
        .omniroute()
        .state();
    let err = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("high")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServerError::VariantOfferUnknown { .. }),
        "{err:?}"
    );
    let router = app(state);
    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({"workspace": ws.display().to_string(), "variant": "high"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reason"], "variant_offer_unknown");
}

fn with_none_tier() -> Value {
    json!({
        "none": {"reasoning_effort": "none"},
        "high": {"reasoning_effort": "high"}
    })
}

// Given a model that really offers a `none` tier
// When `/variant none` runs, then `/variant off`
// Then the first sends reasoning_effort "none" on the wire and the second
//      sends no override
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_none_tier_is_selected_by_name_and_off_clears() {
    let provider = spawn_capturing_provider(vec![done_body(), done_body()]).await;
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .variants(with_none_tier(), Some("high"))
        .state();
    let id = state.create_session(&ws, None, false).await.unwrap();

    let out = state.run_command(id, "variant", "none").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.data.as_ref().unwrap()["variant"], "none");
    assert_eq!(out.render_text(), "variant: none; on offer: high, none");
    assert_eq!(saved(&state, id).await.as_deref(), Some("none"));
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "one").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;

    let out = state.run_command(id, "variant", "off").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(saved(&state, id).await, None);
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "two").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done" && e.seq > 12
    })
    .await;

    let bodies: Vec<Value> = provider
        .bodies()
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(bodies[0]["reasoning_effort"], "none");
    assert!(bodies[1].get("reasoning_effort").is_none(), "{}", bodies[1]);
}

// Given a session on a variant really called `none`, which the alias then
//      stops offering
// When `/new` runs on the same alias
// Then it is refused: the inherited name is looked up exactly, and does not
//      become "no variant"
#[tokio::test]
async fn new_starts_from_assignment_without_inheriting_variant_override() {
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::unreachable(tmp.path()).variants(with_none_tier(), None);
    let state = cfg.state();
    let id = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("none")))
        .await
        .unwrap();

    let out = state.run_command(id, "new", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    let fresh: ulid::Ulid = out.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(saved(&state, fresh).await, None);

    TestServerConfig::unreachable(tmp.path())
        .variants(json!({"high": {"reasoning_effort": "high"}}), None)
        .config();
    let before = state.session_count();
    let out = state.run_command(id, "new", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(
        state.session_count(),
        before + 1,
        "new uses the current assignment rather than the saved override"
    );
}

// Given a models.json found in the workspace (no explicit path)
// When a resume finds the saved variant gone
// Then the error names that workspace file, not the global one
#[tokio::test]
async fn variant_errors_name_the_models_file_that_was_loaded() {
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::unreachable(tmp.path()).variants(offer(), None);
    let mut session = cfg.config().split().0;
    std::fs::copy(cfg.models_path(), ws.join("models.json")).unwrap();
    session.models_path = None;
    let service = withnav::session::Service::new(session).expect("service");
    let id = service
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("low")))
        .await
        .unwrap();
    service.close_session(id).await.unwrap();
    service.history().flush().await.unwrap();

    std::fs::write(
        ws.join("models.json"),
        std::fs::read_to_string(ws.join("models.json"))
            .unwrap()
            .replace("\"low\"", "\"lower\""),
    )
    .unwrap();
    let err = service
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains(&ws.join("models.json").display().to_string()),
        "{text}"
    );
}

async fn serve_chat(server: &wiremock::MockServer) {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    wiremock::Mock::given(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(common::sse::sse_response(&[done]))
        .mount(server)
        .await;
}

fn chat_bodies(requests: Vec<wiremock::Request>) -> Vec<Value> {
    requests
        .into_iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

// Given a session on an explicit models.json variant with several keys, whose
//      map is later removed (the alias now generates its variants), and a
//      provider that is down with nothing cached
// When the session is resumed and takes a turn
// Then it sends the keys it saved, not a rebuilt `reasoning_effort`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_offer_resumes_with_the_saved_override() {
    let server = wiremock::MockServer::start().await;
    serve_chat(&server).await;
    let (tmp, ws) = tmp_ws();
    let url = format!("{}/v1", server.uri());
    let explicit =
        json!({"deep": {"reasoning_effort": "high", "thinking": {"budget_tokens": 8000}}});
    let state = TestServerConfig::mock(tmp.path(), &url)
        .omniroute()
        .variants(explicit, None)
        .state();
    let id = state
        .create_session_with_variant(&ws, None, false, VariantChoice::from_word(Some("deep")))
        .await
        .unwrap();
    state.close_session(id).await.unwrap();
    state.history().flush().await.unwrap();

    // The explicit map is gone; the provider reports nothing.
    TestServerConfig::mock(tmp.path(), &url)
        .omniroute()
        .config();
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resumes with the saved override");
    assert_eq!(
        state.session_summary(id).await.unwrap().variant.as_deref(),
        Some("deep")
    );
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(&state, id, "go").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;

    let bodies = chat_bodies(server.received_requests().await.unwrap());
    let body = bodies.last().expect("a chat request");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["thinking"], json!({"budget_tokens": 8000}));
}

// Given a row saved before the override was kept, and an unreadable offer
// When the session is resumed
// Then it is the retryable 503, not a guessed override
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_row_without_an_override_waits_for_the_offer() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    let (state, id) = stopped_generated(&server, tmp.path(), &ws).await;
    rusqlite::Connection::open(tmp.path().join("history.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET variant_override = NULL WHERE id = ?1",
            [id.to_string()],
        )
        .unwrap();
    std::fs::remove_dir_all(tmp.path().join("data/model-catalog")).unwrap();
    serve_down(&server).await;

    let err = state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServerError::VariantOfferUnknown { .. }),
        "{err:?}"
    );
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"), "kept");
}

// Given a known offer on resume
// Then the saved override is refreshed from the current definition
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_known_offer_refreshes_the_saved_override() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    let (state, id) = stopped_generated(&server, tmp.path(), &ws).await;
    rusqlite::Connection::open(tmp.path().join("history.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET variant_override = NULL WHERE id = ?1",
            [id.to_string()],
        )
        .unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("the offer is readable");
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(
        meta.variant_override.as_deref(),
        Some(r#"{"reasoning_effort":"high"}"#)
    );
}

// Given a session created during a provider outage (its offer unknown)
// When the provider is back and `/variant high` runs in the same session
// Then the offer is read again and the switch succeeds
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variant_works_after_the_provider_recovers() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    serve_down(&server).await;
    let state = TestServerConfig::mock(tmp.path(), &format!("{}/v1", server.uri()))
        .omniroute()
        .state();
    let id = state.create_session(&ws, None, false).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let out = state.run_command(id, "variant", "high").await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("cannot confirm"));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a failed lookup is not retried within the backoff"
    );

    serve_tiers(&server, &["low", "high"]).await;
    state.expire_catalog_backoff();
    let out = state.run_command(id, "variant", "high").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the lookup is retried after expiring the backoff"
    );
    assert_eq!(saved(&state, id).await.as_deref(), Some("high"));
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(
        meta.variant_override.as_deref(),
        Some(r#"{"reasoning_effort":"high"}"#)
    );
    let out = state.run_command(id, "variant", "").await.unwrap();
    assert_eq!(out.data.unwrap()["offered"], json!(["high", "low"]));
}

// Given a session started on one model while its variant offer was unknown
// When the alias is then pointed at another model that has explicit variants,
//      and `/variant high` runs in the same live session
// Then the other model's offer is refused (the session still talks to the
//      first model), and neither the variant nor the saved override changes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reread_offer_of_a_retargeted_alias_is_refused() {
    let server = wiremock::MockServer::start().await;
    let (tmp, ws) = tmp_ws();
    serve_down(&server).await;
    let cfg = TestServerConfig::mock(tmp.path(), &format!("{}/v1", server.uri())).omniroute();
    let state = cfg.state();
    let id = state.create_session(&ws, None, false).await.unwrap();

    let path = cfg.models_path();
    let mut models: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut entry = models["models"]
        .as_object_mut()
        .unwrap()
        .remove("mock/mock-model")
        .unwrap();
    entry["model"] = json!("other-model");
    entry["variants"] = json!({"high": {"reasoning_effort": "high"}});
    models["models"]["mock/other-model"] = entry;
    std::fs::write(&path, models.to_string()).unwrap();

    let out = state.run_command(id, "variant", "high").await.unwrap();
    assert!(!out.ok, "{out:?}");
    let error = out.error.unwrap();
    assert!(error.contains("mock-model"), "{error}");
    let meta = state.history().meta(id).await.unwrap().unwrap();
    assert_eq!(meta.variant, None);
    assert_eq!(meta.variant_override, None);
    let out = state.run_command(id, "variant", "").await.unwrap();
    assert_eq!(out.data.unwrap()["offered"], json!([]));
}
