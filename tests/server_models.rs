//! `GET /v1/models`: canonical identities, all API kinds and safe metadata.

use serde_json::{Value, json};

mod common;
use common::server::{PROVIDER, TestServerConfig, spawn_server};

const SECRET_SENTINEL: &str = "synthetic-secret-never-echo";

async fn catalog_response(
    state: withnav::server::AppState,
    root: &std::path::Path,
) -> (u16, Value) {
    let (addr, task) = spawn_server(state).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .query(&[("workspace", root)])
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap();
    // Do not include response content in failure diagnostics: it may contain credentials.
    assert!(
        !body.to_string().contains(SECRET_SENTINEL),
        "catalog leaked a synthetic secret"
    );
    task.abort();
    (status, body)
}

#[tokio::test]
async fn invalid_settings_and_capabilities_do_not_echo_supplied_values() {
    for field in ["default_agent", "supports_tools", "settings"] {
        let tmp = tempfile::tempdir().unwrap();
        let state = TestServerConfig::unreachable(tmp.path()).state();
        if field == "settings" {
            std::fs::write(
                tmp.path().join("cfg/config.json"),
                json!(SECRET_SENTINEL).to_string(),
            )
            .unwrap();
        } else if field == "default_agent" {
            std::fs::write(
                tmp.path().join("cfg/config.json"),
                json!({"default_agent": {"secret": SECRET_SENTINEL}}).to_string(),
            )
            .unwrap();
        } else {
            let path = tmp.path().join("models.json");
            let mut catalog: Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            catalog["models"]["mock/mock-model"][field] = json!(SECRET_SENTINEL);
            std::fs::write(path, catalog.to_string()).unwrap();
        }
        let (status, _) = catalog_response(state, tmp.path()).await;
        assert_eq!(status, 400);
    }
}

#[tokio::test]
async fn default_agent_must_be_a_valid_agent_id() {
    for (agent, expected) in [("Build", 400), ("build/plan", 400), ("plan", 200)] {
        let tmp = tempfile::tempdir().unwrap();
        let state = TestServerConfig::unreachable(tmp.path()).state();
        std::fs::write(
            tmp.path().join("cfg/config.json"),
            json!({"default_agent": agent}).to_string(),
        )
        .unwrap();
        let (status, body) = catalog_response(state, tmp.path()).await;
        assert_eq!(status, expected);
        if status == 200 {
            assert_eq!(body["default_agent"], agent);
        } else {
            assert!(!body.to_string().contains(agent));
        }
    }
}

#[tokio::test]
async fn malformed_auth_store_diagnostic_does_not_echo_its_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    std::fs::write(
        tmp.path().join("cfg/auth.json"),
        json!(SECRET_SENTINEL).to_string(),
    )
    .unwrap();
    let (status, body) = catalog_response(state, tmp.path()).await;
    assert_eq!(status, 200);
    assert!(
        body["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["disabled_reason"] == "model authentication unavailable")
    );
}

#[tokio::test]
async fn provider_response_diagnostic_does_not_echo_endpoint_or_key() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(503)
                .set_body_string(format!("endpoint={} key={SECRET_SENTINEL}", provider.uri())),
        )
        .mount(&provider)
        .await;
    let state = TestServerConfig::mock(tmp.path(), &format!("{}/v1", provider.uri()))
        .omniroute()
        .state();
    let (status, body) = catalog_response(state, tmp.path()).await;
    assert_eq!(status, 200);
    assert!(
        !body.to_string().contains(&provider.uri()),
        "catalog leaked provider endpoint"
    );
    assert!(body["models"].as_array().unwrap().iter().all(|entry| {
        entry["refresh_error"]
            .as_str()
            .is_some_and(|reason| reason.contains("503"))
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_all_api_kinds_with_canonical_identity_and_configured_default_agent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = TestServerConfig::mock(tmp.path(), "http://127.0.0.1:9").navigator_state();

    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                PROVIDER: {
                    "base_url": "http://127.0.0.1:9",
                    "api_key_env": "WITHNAV_TEST_KEY",
                    "headers": {}
                }
            },
            "models": {
                "mock/combo/mock-opus": { "provider": PROVIDER, "model": "combo/mock-opus",
                               "supports_tools": true, "context_window": 200000 },
                "mock/mock-plain": { "provider": PROVIDER, "model": "mock-plain", "supports_tools": false },
                "mock/mock-fim": { "provider": PROVIDER, "model": "mock-fim", "api": "fim", "supports_tools": true },
                "mock/typesafe/jev": { "provider": PROVIDER, "model": "typesafe/jev", "api": "systemone" }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    std::fs::write(
        tmp.path().join("cfg/config.json"),
        r#"{"default_agent":"plan"}"#,
    )
    .unwrap();

    let (addr, _task) = spawn_server(state).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Editor hooks do not supply a model default; catalog selection is separate.
    let ok = client
        .get(format!("{base}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .expect("get models");
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.expect("models json");
    assert_eq!(body["models"].as_array().unwrap().len(), 4);
    let chat = by_alias(&body, "combo/mock-opus");
    assert_eq!(chat["gateway"], PROVIDER);
    assert_eq!(chat["upstream_id"], "combo/mock-opus");
    assert_eq!(chat["provider_group"], "combo");
    assert_eq!(chat["api"], "chat");
    assert_eq!(chat["supports_tools"], true);
    assert_eq!(chat["context_window"], 200000);
    let fim = by_alias(&body, "mock-fim");
    assert_eq!(fim["provider_group"], PROVIDER);
    assert_eq!(fim["api"], "fim");
    assert_eq!(by_alias(&body, "typesafe/jev")["api"], "systemone");
    assert_eq!(body["default_agent"], "plan");
    assert!(body.get("default").is_none());
    for model in body["models"].as_array().unwrap() {
        assert!(model.get("alias").is_none());
        assert!(model.get("model").is_none());
        assert!(model.get("provider").is_none());
    }

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
            "providers": { PROVIDER: { "base_url": base_url, "api_key_env": "WITHNAV_TEST_KEY", "headers": {} } },
            "models": {
                "mock/m-found": { "provider": PROVIDER, "model": "m-found", "supports_tools": true },
                "mock/m-pinned": { "provider": PROVIDER, "model": "m-pinned", "supports_tools": true,
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
        .find(|m| m["name"] == format!("{PROVIDER}/{alias}"))
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
            }, {"id": "m-pinned", "context_length": 1_000_000,
                "max_input_tokens": 900_000, "max_output_tokens": 128_000}]})),
        )
        .mount(&provider)
        .await;
    let body = get_models(tmp.path(), &format!("{}/v1", provider.uri())).await;

    assert_eq!(
        by_alias(&body, "m-found"),
        &json!({
            "name": format!("{PROVIDER}/m-found"), "gateway": PROVIDER,
            "provider_group": PROVIDER, "upstream_id": "m-found", "api": "chat",
            "supports_tools": true, "supports_parallel_tool_calls": null, "context_window": 1_000_000,
            "enabled": true, "window_source": "discovered.context_length", "stale": false,
            "reported_output": 128_000, "max_input_tokens": 900_000
        })
    );
    assert_eq!(
        by_alias(&body, "m-pinned"),
        &json!({
            "name": format!("{PROVIDER}/m-pinned"), "gateway": PROVIDER,
            "provider_group": PROVIDER, "upstream_id": "m-pinned", "api": "chat",
            "supports_tools": true, "supports_parallel_tool_calls": null, "context_window": 200_000,
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
    let found = by_alias(&body, "m-found");
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

/// Two distinct OmniRoute models, with and without a pinned window/default.
fn write_variant_models(root: &std::path::Path, base_url: &str) {
    std::fs::write(
        root.join("models.json"),
        json!({
            "providers": { PROVIDER: { "base_url": base_url, "api_key_env": "WITHNAV_TEST_KEY",
                                       "headers": {}, "catalog_format": "omniroute" } },
            "models": {
                "mock/m-tiers": { "provider": PROVIDER, "model": "m-tiers", "supports_tools": true,
                            "context_window": 200000, "default_variant": "high" },
                "mock/m-plain": { "provider": PROVIDER, "model": "m-plain", "supports_tools": true }
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
            }, {"id": "m-plain", "context_length": 1_000_000,
                "capabilities": {"effort_tiers": ["low", "high"]}}]})),
        )
        .mount(&provider)
        .await;
    let body = get_models_from(
        tmp.path(),
        &format!("{}/v1", provider.uri()),
        write_variant_models,
    )
    .await;

    let pinned = by_alias(&body, "m-tiers");
    assert_eq!(pinned["variants"], json!(["high", "low"]));
    assert_eq!(pinned["default_variant"], "high");
    assert!(pinned.get("default_variant_unavailable").is_none());
    assert_eq!(
        by_alias(&body, "m-plain")["variants"],
        json!(["high", "low"])
    );
    assert!(by_alias(&body, "m-plain").get("default_variant").is_none());
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

    let pinned = by_alias(&body, "m-tiers");
    assert_eq!(pinned["enabled"], true);
    assert_eq!(pinned["context_window"], 200_000);
    assert!(pinned["refresh_error"].as_str().unwrap().contains("503"));
    assert!(pinned.get("variants").is_none());
    assert!(pinned.get("default_variant").is_none(), "{pinned}");
    assert_eq!(pinned["default_variant_unavailable"], "high");
}

#[tokio::test]
async fn duplicate_gateway_upstream_identity_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {"mock": {"base_url": "http://127.0.0.1:1/v1"}},
            "models": {
                "first": {"provider":"mock","model":"combo/same","context_window":200000},
                "second": {"provider":"mock","model":"combo/same","context_window":100000}
            }
        })
        .to_string(),
    )
    .unwrap();
    let (addr, task) = spawn_server(state).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("model catalog configuration unavailable"),
        "{body}"
    );
    assert!(!body.contains("127.0.0.1"));
    task.abort();
}

#[tokio::test]
async fn identical_upstream_ids_on_distinct_gateways_are_distinct_models() {
    let tmp = tempfile::tempdir().unwrap();
    let state = TestServerConfig::unreachable(tmp.path()).state();
    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                "first": {"base_url": "http://127.0.0.1:1/v1"},
                "second": {"base_url": "http://127.0.0.1:2/v1"}
            },
            "models": {
                "first/vendor/same": {"provider":"first","model":"vendor/same","api":"fim"},
                "second/vendor/same": {"provider":"second","model":"vendor/same","api":"fim"}
            }
        })
        .to_string(),
    )
    .unwrap();
    let (addr, task) = spawn_server(state).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .query(&[("workspace", tmp.path())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["name"], "first/vendor/same");
    assert_eq!(models[1]["name"], "second/vendor/same");
    assert_eq!(models[0]["provider_group"], "vendor");
    assert_eq!(body["default_agent"], "build");
    let text = body.to_string();
    assert!(!text.contains("base_url") && !text.contains("headers") && !text.contains("127.0.0.1"));
    task.abort();
}
