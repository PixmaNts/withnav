//! Wire-body lock for model variants: a variant's overrides are merged into
//! the chat request body, and with none the body is byte-identical to the
//! one sent before variants existed. Capture-if-absent, then byte equality.

use std::collections::BTreeMap;
use std::path::PathBuf;

use navai::agent::prompt::build_request;
use navai::provider::client::ModelClient;
use navai::provider::config::{default_variant, effective_variants, ModelsConfig, ProviderConfig};
use navai::provider::openai::ClientCache;
use navai::provider::openai::ChatMessage;
use tokio_util::sync::CancellationToken;

mod common;
use common::sse::{http_sse, spawn_capturing_provider, sse_body};

const FIXTURE_DIR: &str = "tests/fixtures/variants_wire";
const DONE: &str = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;

fn lock_fixture(name: &str, body: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    match std::fs::read_to_string(&path) {
        Ok(locked) => assert_eq!(locked, body, "wire body drifted from fixture {name}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&path, body).unwrap();
        }
        Err(e) => panic!("failed reading fixture {name}: {e}"),
    }
}

const MODELS: &str = r#"{
  "providers": {"p": {"base_url": "http://unused/v1", "catalog_format": "omniroute"}},
  "models": {
    "plain": {"provider": "p", "model": "m", "variants": {}},
    "deep": {"provider": "p", "model": "m", "default_variant": "high"},
    "nested": {"provider": "p", "model": "m", "variants": {
      "think": {"thinking": {"type": "enabled", "budget_tokens": 8000}, "reasoning_effort": "high"}
    }}
  }
}"#;

/// Sends one request through `ModelClient::chat` and returns the body the
/// provider received.
async fn send(variant: Option<&navai::provider::config::Variant>) -> String {
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

fn tiers(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn low_high_and_max_each_add_their_reasoning_effort() {
    let cfg = ModelsConfig::parse(MODELS).unwrap();
    let resolved = cfg.resolve("deep").unwrap();
    let variants = effective_variants(&resolved, &tiers(&["low", "high", "max"]));
    for tier in ["low", "high", "max"] {
        let body = send(Some(&variants[tier])).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["reasoning_effort"], tier);
        assert_eq!(parsed["model"], "test-model", "the model is never replaced");
        lock_fixture(&format!("effort_{tier}"), &body);
    }
}

#[tokio::test]
async fn a_nested_options_object_is_sent_as_is() {
    let cfg = ModelsConfig::parse(MODELS).unwrap();
    let resolved = cfg.resolve("nested").unwrap();
    let variants = effective_variants(&resolved, &[]);
    let body = send(Some(&variants["think"])).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["thinking"]["type"], "enabled");
    assert_eq!(parsed["thinking"]["budget_tokens"], 8000);
    assert_eq!(parsed["reasoning_effort"], "high");
    lock_fixture("nested_options", &body);
}

#[tokio::test]
async fn the_alias_default_is_applied_when_the_caller_passes_it() {
    let cfg = ModelsConfig::parse(MODELS).unwrap();
    let resolved = cfg.resolve("deep").unwrap();
    let (name, variant) = default_variant(&resolved, &tiers(&["low", "high"]))
        .unwrap()
        .expect("deep has a default");
    assert_eq!(name, "high");
    let body = send(Some(&variant)).await;
    assert_eq!(
        body,
        send(Some(&effective_variants(&resolved, &tiers(&["high"]))["high"])).await
    );
    lock_fixture("default_applied", &body);
}

#[tokio::test]
async fn no_variant_leaves_the_body_byte_identical_to_the_unvaried_request() {
    let plain = send(None).await;
    assert_eq!(plain, send(Some(&Default::default())).await);
    let cfg = ModelsConfig::parse(MODELS).unwrap();
    let resolved = cfg.resolve("plain").unwrap();
    assert!(effective_variants(&resolved, &tiers(&["low"])).is_empty());
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
