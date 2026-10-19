//! Wire-golden lock for the `fim` API kind: the exact request body sent to
//! `POST {base}/fim/completions`, the `choices[0].text` answer parsing, the
//! bearer header, and the error-status path — against a wiremock server.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use withnav_provider::auth;
use withnav_provider::config::{
    ApiKind, ModelConfig, ModelsConfig, ProviderConfig, ReasoningHistory, ResolvedModel,
};
use withnav_provider::error::ProviderError;
use withnav_provider::fim::InceptionFim;
use withnav_provider::openai::ClientCache;

fn resolved(base_url: String) -> ResolvedModel {
    ResolvedModel {
        alias: "reflex".to_string(),
        provider_name: "inception".to_string(),
        provider: ProviderConfig {
            base_url,
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            // Long on purpose: the reflex engine must clamp it.
            request_timeout_ms: 600_000,
            catalog_format: None,
        },
        model: ModelConfig {
            provider: "inception".to_string(),
            model: "mercury-edit-2".to_string(),
            api: ApiKind::Fim,
            context_window: None,
            max_input_tokens: None,
            max_message_tokens: None,
            max_output_tokens: None,
            supports_tools: false,
            supports_parallel_tool_calls: None,
            images: None,
            thinking: None,
            default_variant: None,
            variants: None,
            reasoning_history: ReasoningHistory::Preserve,
        },
    }
}

fn engine(base_url: String, api_key: &str) -> InceptionFim {
    InceptionFim::new(
        resolved(base_url),
        api_key.to_string(),
        Arc::new(ClientCache::new()),
    )
}

const PROMPT: &str = "fn main() {\n    ";
const SUFFIX: &str = "\n}";
const MAX_TOKENS: u64 = 128;

async fn mount_fim_mock(
    server: &MockServer,
    key: &str,
    expected: Option<serde_json::Value>,
    response: impl wiremock::Respond + 'static,
) {
    let mut mock = Mock::given(method("POST"))
        .and(path("/fim/completions"))
        .and(header("authorization", format!("Bearer {key}")));
    if let Some(body) = expected {
        mock = mock.and(body_partial_json(body));
    }
    mock.respond_with(response).expect(1).mount(server).await;
}

/// The exact wire body the Inception endpoint must receive.
#[tokio::test]
async fn golden_request_body_and_text_parsing() {
    let server = MockServer::start().await;
    mount_fim_mock(
        &server,
        "test-key",
        Some(json!({
            "model": "mercury-edit-2",
            "prompt": "fn main() {\n    ",
            "suffix": "\n}",
            "max_tokens": 128
        })),
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"text": "println!(\"hi\");"}],
            "usage": {"prompt_tokens": 41, "completion_tokens": 7}
        })),
    )
    .await;

    let engine = engine(server.uri(), "test-key");
    let cancel = tokio_util::sync::CancellationToken::new();
    let answer = engine
        .complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel)
        .await
        .unwrap();
    assert_eq!(answer.text, "println!(\"hi\");");
    assert_eq!(answer.usage.prompt_tokens, 41);
    assert_eq!(answer.usage.completion_tokens, 7);
}

/// The temperature field is part of the frozen wire shape.
#[tokio::test]
async fn golden_body_carries_temperature() {
    let server = MockServer::start().await;
    mount_fim_mock(
        &server,
        "test-key",
        Some(json!({"temperature": 0.1})),
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"text": "x"}]
        })),
    )
    .await;

    let engine = engine(server.uri(), "test-key");
    let cancel = tokio_util::sync::CancellationToken::new();
    let answer = engine
        .complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel)
        .await
        .unwrap();
    assert_eq!(answer.text, "x");
    // Usage absent in the answer: zeroed, not an error.
    assert_eq!(answer.usage.prompt_tokens, 0);
}

/// A non-2xx status surfaces as a Api error
/// carrying the status and the endpoint's error message.
#[tokio::test]
async fn error_status_is_a_provider_error() {
    let server = MockServer::start().await;
    mount_fim_mock(
        &server,
        "test-key",
        None,
        ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "rate limited"}
        })),
    )
    .await;

    let engine = engine(server.uri(), "test-key");
    let cancel = tokio_util::sync::CancellationToken::new();
    let err = engine
        .complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel)
        .await
        .unwrap_err();
    match &err {
        ProviderError::Api { status: 429, .. } => {
            assert!(err.to_string().contains("rate limited"), "got: {err}");
        }
        other => panic!("expected provider error, got {other:?}"),
    }
}

/// A 200 whose body is not the FIM shape is a Malformed error, not a
/// crash and not silence.
#[tokio::test]
async fn malformed_body_is_a_malformed_error() {
    let server = MockServer::start().await;
    mount_fim_mock(
        &server,
        "test-key",
        None,
        ResponseTemplate::new(200).set_body_json(json!({
            "unexpected": true
        })),
    )
    .await;

    let engine = engine(server.uri(), "test-key");
    let cancel = tokio_util::sync::CancellationToken::new();
    let err = engine
        .complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::MalformedResponse(_)));
}

/// Cancelling before the answer returns promptly with a cancellation
/// error (the token is honoured mid-request).
#[tokio::test]
async fn cancelled_call_stops_waiting() {
    let server = MockServer::start().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    mount_fim_mock(&server, "test-key", None, move |_: &wiremock::Request| {
        // In October 2026, a timer could cancel before the request reached the mock under
        // load. Cancel only after receipt, while the delayed response is still outstanding.
        trigger.cancel();
        ResponseTemplate::new(200)
            .set_body_json(json!({
                "choices": [{"text": "late"}]
            }))
            .set_delay(Duration::from_secs(10))
    })
    .await;

    let engine = engine(server.uri(), "test-key");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        engine.complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel),
    )
    .await
    .expect("cancellation stops waiting for the delayed response");
    assert!(matches!(result, Err(ProviderError::Canceled { .. })));
}

/// The repo's models.json resolves the `reflex` alias exactly as this
/// engine expects (inception provider, mercury-edit-2, fim api kind).
#[test]
fn repo_models_json_resolves_the_reflex_alias() {
    let cfg = ModelsConfig::parse(include_str!("../../../models.example.json")).unwrap();
    let r = cfg.resolve("inception/mercury-edit-2").unwrap();
    assert_eq!(r.provider_name, "inception");
    assert_eq!(r.model.model, "mercury-edit-2");
    assert_eq!(r.model.api, ApiKind::Fim);
    assert_eq!(r.provider.base_url, "https://api.inceptionlabs.ai/v1");
}

/// The api key resolves from a 0600 auth.json store the way the binary
/// does (auth entry name = provider name).
#[tokio::test]
async fn api_key_resolves_from_the_auth_store() {
    let tmp = tempfile::tempdir().unwrap();
    auth::set_key(tmp.path(), "inception", "golden-test-key").unwrap();
    let key = auth::resolve_key(tmp.path(), "inception", Some("INCEPTION_API_KEY")).unwrap();
    assert_eq!(key, "golden-test-key");

    // And the engine built with it sends it as the bearer token.
    let server = MockServer::start().await;
    mount_fim_mock(
        &server,
        "golden-test-key",
        None,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"text": "ok"}]
        })),
    )
    .await;
    let engine = engine(server.uri(), &key);
    let cancel = tokio_util::sync::CancellationToken::new();
    let answer = engine
        .complete(PROMPT, SUFFIX, MAX_TOKENS, &cancel)
        .await
        .unwrap();
    assert_eq!(answer.text, "ok");
}
