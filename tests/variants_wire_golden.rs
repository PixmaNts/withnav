//! Wire-body lock for model variants: a variant's overrides are merged into
//! the chat request body, and with none the body is byte-identical to the
//! one sent before variants existed. Checked-in fixtures are required.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use withnav::agent::prompt::build_request;
use withnav::provider::catalog::{ModelCatalog, ResolvedChat, resolve_chat};
use withnav::provider::client::ModelClient;
use withnav::provider::config::{ModelsConfig, ProviderConfig};
use withnav::provider::openai::ChatMessage;
use withnav::provider::openai::ClientCache;

mod common;
use common::sse::{http_sse, spawn_capturing_provider, sse_body};

const FIXTURE_DIR: &str = "tests/fixtures/variants_wire";
const DONE: &str = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;

fn lock_fixture(name: &str, body: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    let path = dir.join(format!("{name}.json"));
    let locked = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("required golden fixture {}: {e}", path.display()));
    assert_eq!(locked, body, "wire body drifted from fixture {name}");
}

/// A models.json with three aliases on one OmniRoute provider at `base_url`.
fn models(base_url: &str) -> String {
    format!(
        r#"{{
  "providers": {{"p": {{"base_url": "{base_url}", "catalog_format": "omniroute"}}}},
  "models": {{
    "p/plain": {{"provider": "p", "model": "plain", "variants": {{}}}},
    "p/deep": {{"provider": "p", "model": "deep", "default_variant": "high"}},
    "p/nested": {{"provider": "p", "model": "nested", "variants": {{
      "think": {{"thinking": {{"type": "enabled", "budget_tokens": 8000}}, "reasoning_effort": "high"}}
    }}}}
  }}
}}"#
    )
}

/// Resolve every scenario through one provider catalogue and temporary cache.
async fn resolved_models() -> BTreeMap<String, ResolvedChat> {
    let provider = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"data": [
                {"id": "deep", "context_length": 8000, "capabilities": {"effort_tiers": ["low", "high", "max"]}},
                {"id": "nested", "context_length": 8000, "capabilities": {"effort_tiers": []}},
                {"id": "plain", "context_length": 8000, "capabilities": {"effort_tiers": ["low"]}}
            ]}),
        ))
        .expect(1)
        .mount(&provider)
        .await;
    let cfg = ModelsConfig::parse(&models(&format!("{}/v1", provider.uri()))).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let clients = ClientCache::new();
    let mut chats = BTreeMap::new();
    for alias in ["deep", "nested", "plain"] {
        let resolved = cfg.resolve(&format!("p/{alias}")).unwrap();
        chats.insert(
            alias.into(),
            resolve_chat(&catalog, &resolved, "k", &clients)
                .await
                .unwrap(),
        );
    }
    chats
}

/// Sends one request through `ModelClient::chat` and returns the body the
/// provider received.
async fn send(variant: Option<&withnav::provider::config::Variant>) -> String {
    let server = spawn_capturing_provider(vec![http_sse(&sse_body(&[DONE]))]).await;
    let provider = ProviderConfig {
        base_url: server.url.clone(),
        api_key_env: None,
        headers: BTreeMap::new(),
        connect_timeout_ms: 2000,
        request_timeout_ms: 600_000,
        catalog_format: None,
    };
    let client = ModelClient::new("p", &provider, "k".into(), &ClientCache::new()).unwrap();
    let request = build_request(
        "test-model",
        None,
        None,
        None,
        vec![ChatMessage::User {
            provenance: Default::default(),
            content: "hello".into(),
        }],
        Vec::new(),
        Some(256),
    );
    let message = client
        .chat(request, variant, &CancellationToken::new(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(message.text, "ok");
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    bodies[0].clone()
}

#[tokio::test]
async fn variants_preserve_their_golden_wire_bodies() {
    let chats = resolved_models().await;
    let deep = &chats["deep"];
    let (name, default) = deep
        .default_override("deep")
        .unwrap()
        .expect("deep has a default");
    assert_eq!(name, "high");
    let mut bodies = BTreeMap::new();
    for (fixture, variant, expected) in [
        (
            "effort_low",
            &deep.variants["low"],
            serde_json::json!({"reasoning_effort": "low"}),
        ),
        (
            "effort_high",
            &deep.variants["high"],
            serde_json::json!({"reasoning_effort": "high"}),
        ),
        (
            "effort_max",
            &deep.variants["max"],
            serde_json::json!({"reasoning_effort": "max"}),
        ),
        (
            "nested_options",
            &chats["nested"].variants["think"],
            serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 8000}, "reasoning_effort": "high"}),
        ),
        (
            "default_applied",
            &default,
            serde_json::json!({"reasoning_effort": "high"}),
        ),
    ] {
        let body = send(Some(variant)).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&parsed[key], value, "fixture={fixture}, key={key}");
        }
        assert_eq!(parsed["model"], "test-model", "the model is never replaced");
        lock_fixture(fixture, &body);
        bodies.insert(fixture, body);
    }
    assert_eq!(bodies["default_applied"], bodies["effort_high"]);
    let plain = send(None).await;
    assert_eq!(plain, send(Some(&Default::default())).await);
    assert!(chats["plain"].variants.is_empty());
    // The same request as the Phase-1 lock: system contract, user "hello".
    let locked: Vec<String> = serde_json::from_slice(
        &std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/phase1_wire/no_tool_completion.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plain, locked[0]);
}
