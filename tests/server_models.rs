//! `GET /v1/models`: chat models with tools from the models.json a new
//! session would load; 400 without `workspace`.

use serde_json::{json, Value};

mod common;
use common::server::{spawn_server, TestServerConfig, PROVIDER};

// Given a models.json with a chat+tools alias, a chat alias without tools
//      and a fim alias
// When GET /v1/models?workspace=... runs
// Then only the chat+tools alias is listed, with its context window, and
//      the navigator default is "cognition"; without workspace it is a 400
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_chat_models_with_tools_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = TestServerConfig::mock(tmp.path(), "http://127.0.0.1:9").navigator_state();

    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                PROVIDER: {
                    "base_url": "http://127.0.0.1:9",
                    "api_key_env": "NAVAI_TEST_KEY",
                    "headers": {}
                }
            },
            "models": {
                "cognition": { "provider": PROVIDER, "model": "mock-opus",
                               "supports_tools": true, "context_window": 200000 },
                "plain": { "provider": PROVIDER, "model": "mock-plain", "supports_tools": false },
                "fill": { "provider": PROVIDER, "model": "mock-fim", "api": "fim", "supports_tools": true }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    let (addr, _task) = spawn_server(state).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // With a workspace: only the chat+tools alias, and the navigator default.
    let ok = client
        .get(format!("{base}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .expect("get models");
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.expect("models json");
    assert_eq!(
        body["models"],
        json!([{ "alias": "cognition", "model": "mock-opus", "context_window": 200000,
                 "enabled": true, "window_source": "models.json", "stale": false }])
    );
    assert_eq!(body["default"], "cognition");

    // Without a workspace: 400.
    let missing = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("get models without workspace");
    assert_eq!(missing.status(), 400);
}

fn write_models(root: &std::path::Path, base_url: &str) {
    std::fs::write(
        root.join("models.json"),
        json!({
            "providers": { PROVIDER: { "base_url": base_url, "api_key_env": "NAVAI_TEST_KEY", "headers": {} } },
            "models": {
                "found": { "provider": PROVIDER, "model": "m-found", "supports_tools": true },
                "pinned": { "provider": PROVIDER, "model": "m-found", "supports_tools": true,
                            "context_window": 200000 }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
}

/// Serves a fresh state over `root` (building it writes a default
/// models.json, so ours goes in after) and asks for the models.
async fn get_models(root: &std::path::Path, base_url: &str) -> Value {
    get_models_from(root, base_url, write_models).await
}

async fn get_models_from(
    root: &std::path::Path,
    base_url: &str,
    write: fn(&std::path::Path, &str),
) -> Value {
    let state = TestServerConfig::mock(root, base_url).navigator_state();
    write(root, base_url);
    let (addr, task) = spawn_server(state).await;
    let body = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .query(&[("workspace", root)])
        .send()
        .await
        .expect("get models")
        .json()
        .await
        .expect("models json");
    task.abort();
    body
}

fn by_alias<'a>(body: &'a Value, alias: &str) -> &'a Value {
    body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["alias"] == alias)
        .unwrap_or_else(|| panic!("no {alias} in {body}"))
}

// Given an alias without context_window and one with a different override,
//      both on a model the provider reports
// When GET /v1/models runs
// Then each entry shows its window source, the reported limits, and the
//      override shows the discrepancy; nothing secret or variant-like appears
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_the_source_limits_and_discrepancy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(json!({"data": [{
                "id": "m-found", "context_length": 1_000_000,
                "max_input_tokens": 900_000, "max_output_tokens": 128_000
            }]})),
        )
        .mount(&provider)
        .await;
    let body = get_models(tmp.path(), &format!("{}/v1", provider.uri())).await;

    assert_eq!(
        by_alias(&body, "found"),
        &json!({
            "alias": "found", "model": "m-found", "context_window": 1_000_000,
            "enabled": true, "window_source": "discovered.context_length", "stale": false,
            "reported_output": 128_000, "max_input_tokens": 900_000
        })
    );
    assert_eq!(
        by_alias(&body, "pinned"),
        &json!({
            "alias": "pinned", "model": "m-found", "context_window": 200_000,
            "enabled": true, "window_source": "models.json", "stale": false,
            "reported_output": 128_000, "max_input_tokens": 900_000,
            "discrepancy": { "configured": 200_000, "discovered": 1_000_000 }
        })
    );
    let text = body.to_string();
    assert!(!text.contains("test-key") && !text.contains("variant"));
}

// Given a cache file past its TTL and a provider that now fails
// When GET /v1/models runs
// Then the alias is still enabled on the old values, marked stale with the
//      age of the file and a short error without the endpoint or key
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_refresh_shows_stale_values_with_age_and_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(json!({"data": [{
                "id": "m-found", "context_length": 1_000_000
            }]})),
        )
        .mount(&provider)
        .await;
    let base = format!("{}/v1", provider.uri());
    get_models(tmp.path(), &base).await; // first fetch writes the cache file

    // Age the file by three days and break the provider.
    let dir = tmp.path().join("data").join("model-catalog");
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut cache: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let old = cache["fetched_at"].as_u64().unwrap() - 3 * 86_400;
    cache["fetched_at"] = json!(old);
    std::fs::write(&file, cache.to_string()).unwrap();
    provider.reset().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&provider)
        .await;

    let body = get_models(tmp.path(), &base).await;
    let found = by_alias(&body, "found");
    assert_eq!(found["enabled"], true);
    assert_eq!(found["context_window"], 1_000_000);
    assert_eq!(found["stale"], true);
    assert!(found["stale_age_secs"].as_u64().unwrap() >= 3 * 86_400);
    let error = found["refresh_error"].as_str().unwrap();
    assert!(
        error.contains("503") && !error.contains("127.0.0.1"),
        "{error}"
    );
}

/// Two OmniRoute aliases on one model: `pinned` with an explicit window and
/// a default variant, `plain` with neither.
fn write_variant_models(root: &std::path::Path, base_url: &str) {
    std::fs::write(
        root.join("models.json"),
        json!({
            "providers": { PROVIDER: { "base_url": base_url, "api_key_env": "NAVAI_TEST_KEY",
                                       "headers": {}, "catalog_format": "omniroute" } },
            "models": {
                "pinned": { "provider": PROVIDER, "model": "m-tiers", "supports_tools": true,
                            "context_window": 200000, "default_variant": "high" },
                "plain": { "provider": PROVIDER, "model": "m-tiers", "supports_tools": true }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
}

// Given an OmniRoute provider reporting effort tiers for the model
// When GET /v1/models runs
// Then the generated variants are listed by name and the default is named,
//      without any override object
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_variant_names_and_the_default_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(json!({"data": [{
                "id": "m-tiers", "context_length": 1_000_000,
                "capabilities": {"effort_tiers": ["low", "high"]}
            }]})),
        )
        .mount(&provider)
        .await;
    let body = get_models_from(
        tmp.path(),
        &format!("{}/v1", provider.uri()),
        write_variant_models,
    )
    .await;

    let pinned = by_alias(&body, "pinned");
    assert_eq!(pinned["variants"], json!(["high", "low"]));
    assert_eq!(pinned["default_variant"], "high");
    assert!(pinned.get("default_variant_unavailable").is_none());
    assert_eq!(by_alias(&body, "plain")["variants"], json!(["high", "low"]));
    assert!(by_alias(&body, "plain").get("default_variant").is_none());
    assert!(
        !body.to_string().contains("reasoning_effort"),
        "no override object is exposed: {body}"
    );
}

// Given an alias with a pinned window, no variants and a default variant,
//      and a provider that is down with nothing cached
// When GET /v1/models runs
// Then the alias is enabled on its window, the lookup error is shown, no
//      variants are listed and the default is reported unavailable instead of
//      being advertised
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_alias_with_the_provider_down_reports_its_default_unavailable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&provider)
        .await;
    let body = get_models_from(
        tmp.path(),
        &format!("{}/v1", provider.uri()),
        write_variant_models,
    )
    .await;

    let pinned = by_alias(&body, "pinned");
    assert_eq!(pinned["enabled"], true);
    assert_eq!(pinned["context_window"], 200_000);
    assert!(pinned["refresh_error"].as_str().unwrap().contains("503"));
    assert!(pinned.get("variants").is_none());
    assert!(pinned.get("default_variant").is_none(), "{pinned}");
    assert_eq!(pinned["default_variant_unavailable"], "high");
}
