//! Wire-golden lock for the System One endpoint: request body and auth
//! against a scripted mock (same technique as `tests/phase1_wire_golden.rs`),
//! with the answer shape from
//! `tests/fixtures/navigator/systemone_multi.json`.

use std::collections::BTreeMap;

use rust_agent::error::ProviderError;
use rust_agent::navigator::models::DecisionAdapter;
use rust_agent::navigator::TriggerInputs;
use rust_agent::provider::config::{ApiKind, ModelConfig, ProviderConfig};
use rust_agent::provider::openai::ClientCache;
use rust_agent::provider::systemone::{Answer, DecisionRequest, Question, SystemOneClient};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE_RESPONSE: &str = include_str!("fixtures/navigator/systemone_multi.json");

fn provider_at(base: String) -> ProviderConfig {
    ProviderConfig {
        api_key_env: None,
        base_url: base,
        headers: Default::default(),
        connect_timeout_ms: 5000,
        // Generous configured timeout; the per-layer ~2 s deadline must win.
        request_timeout_ms: 600_000,
    }
}

fn model_config() -> ModelConfig {
    ModelConfig {
        provider: "omniroute".to_string(),
        model: "typesafe/jev-1.13".to_string(),
        api: ApiKind::SystemOne,
        context_window: Some(64_000),
        max_output_tokens: Some(2048),
        supports_tools: false,
        supports_parallel_tool_calls: None,
        thinking: None,
        reasoning_history: Default::default(),
    }
}

fn empty_request() -> DecisionRequest<'static> {
    static EMPTY: std::collections::BTreeMap<String, Question> = std::collections::BTreeMap::new();
    DecisionRequest {
        state: "state",
        questions: &EMPTY,
    }
}

/// A client for `server` with its key in a temp auth store (never printed);
/// the temp dir must outlive the client.
fn client_for(
    server: &MockServer,
    cache: std::sync::Arc<ClientCache>,
) -> (tempfile::TempDir, SystemOneClient) {
    let tmp = tempfile::tempdir().unwrap();
    rust_agent::provider::auth::set_key(tmp.path(), "omniroute", "test-key").unwrap();
    let client = SystemOneClient::new(
        "omniroute",
        &provider_at(server.uri().to_string() + "/v1"),
        &model_config(),
        tmp.path(),
        cache,
    )
    .unwrap();
    (tmp, client)
}

/// The captured response body: the fixture's `response` object verbatim.
fn captured_response_body() -> String {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE_RESPONSE).unwrap();
    serde_json::to_string(&fixture["response"]).unwrap()
}

#[tokio::test]
async fn posts_model_state_questions_and_parses_answers() {
    let server = MockServer::start().await;
    let response_body = captured_response_body();
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(serde_json::json!({
            "model": "typesafe/jev-1.13",
            "state": "Intent: add retry",
            "questions": {
                "worth_attention": { "type": "noul" },
                "route": { "type": "choice" }
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
        .expect(1)
        .mount(&server)
        .await;

    let (_tmp, client) = client_for(&server, std::sync::Arc::new(ClientCache::new()));

    let mut questions = BTreeMap::new();
    questions.insert(
        "worth_attention".to_string(),
        Question::Noul {
            instructions: "Worth a remark?".to_string(),
        },
    );
    let mut criteria = BTreeMap::new();
    criteria.insert("silent".to_string(), "Nothing".to_string());
    criteria.insert("cognition".to_string(), "Deep".to_string());
    questions.insert(
        "route".to_string(),
        Question::Choice {
            instructions: "Who?".to_string(),
            criteria,
        },
    );

    let answers = client
        .decide(
            &DecisionRequest {
                state: "Intent: add retry",
                questions: &questions,
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .answers;

    // The captured answers came back typed.
    assert_eq!(answers.len(), 5);
    match answers.get("worth_attention").unwrap() {
        Answer::Noul { noul } => assert!((noul - 0.8).abs() < 1e-9),
        other => panic!("expected noul, got {other:?}"),
    }
    match answers.get("route").unwrap() {
        Answer::Choice { choice, .. } => assert_eq!(choice, "cognition"),
        other => panic!("expected choice, got {other:?}"),
    }
    match answers.get("severity").unwrap() {
        Answer::Score { score, .. } => assert!((score - 1.56).abs() < 1e-9),
        other => panic!("expected score, got {other:?}"),
    }
    server.verify().await;
}

#[tokio::test]
async fn non_2xx_is_a_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string(r#"{"error":{"message":"model overloaded"}}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (_tmp, client) = client_for(&server, std::sync::Arc::new(ClientCache::new()));

    let err = client
        .decide(
            &empty_request(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("500") || msg.contains("overloaded"), "{msg}");
}

#[tokio::test]
async fn garbage_body_is_malformed_not_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .expect(1)
        .mount(&server)
        .await;

    let (_tmp, client) = client_for(&server, std::sync::Arc::new(ClientCache::new()));

    let err = client
        .decide(
            &empty_request(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::MalformedResponse(_)), "{err}");
}

#[tokio::test]
async fn slow_body_hits_the_two_second_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(captured_response_body())
                // Slower than the ~2 s instinct deadline.
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (_tmp, client) = client_for(&server, std::sync::Arc::new(ClientCache::new()));

    let started = std::time::Instant::now();
    let err = client
        .decide(
            &empty_request(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "deadline not applied, took {elapsed:?}"
    );
    assert!(!err.to_string().is_empty());
}

#[tokio::test]
async fn client_is_reused_across_calls_through_the_cache() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(captured_response_body()))
        .expect(3)
        .mount(&server)
        .await;

    let cache = std::sync::Arc::new(ClientCache::new());
    let (_tmp, client) = client_for(&server, cache.clone());
    let (_tmp2, client2) = client_for(&server, cache);

    for c in [&client, &client2, &client] {
        c.decide(
            &empty_request(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    }
    // Three calls, one provider: the cache handed out one shared client.
    server.verify().await;
}

#[tokio::test]
async fn pipeline_over_systemone_end_to_end() {
    // The pipeline drives the real client against the mock end to end.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(captured_response_body()))
        .expect(1)
        .mount(&server)
        .await;

    let (_tmp, client) = client_for(&server, std::sync::Arc::new(ClientCache::new()));

    let pipeline = rust_agent::navigator::Pipeline::new(
        rust_agent::navigator::PipelineSnapshot::builtin().unwrap(),
    );
    let vars: BTreeMap<String, String> = [("file".to_string(), "a.rs".to_string())]
        .into_iter()
        .collect();
    let _ = TriggerInputs {
        changed_lines: 5,
        since_last_remark_ms: 60_000,
        has_errors: false,
        has_intent: false,
        has_conversation: false,
    };
    // The built-in default asks 3 questions; the captured fixture answers
    // them (worth 0.8, route cognition, drift 0.54) -> no remark branch
    // matches (route != metacognition, drift <= 0.8) -> silent.
    let eval = pipeline
        .evaluate(
            rust_agent::navigator::TriggerKind::Save,
            TriggerInputs {
                changed_lines: 5,
                since_last_remark_ms: 60_000,
                has_errors: false,
                has_intent: true,
                has_conversation: false,
            },
            &vars,
            &DecisionAdapter(client),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(eval.requested_action.as_str(), "silent");
    assert!(eval.answers.is_some());
    server.verify().await;
}
