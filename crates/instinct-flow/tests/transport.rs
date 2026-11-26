use instinct_flow::{protocol::*, *};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    matchers::{body_json, method, path},
    Mock, MockServer, ResponseTemplate,
};
fn questions() -> Questions {
    Questions::from([(
        "go".into(),
        Question::Noul {
            instructions: "Go?".into(),
        },
    )])
}
fn client(server: &MockServer) -> SystemOneClient {
    SystemOneClient::new(
        format!("{}/systemone", server.uri()).parse().unwrap(),
        "model".into(),
        "secret".into(),
        Duration::from_secs(2),
        BTreeMap::new(),
    )
    .unwrap()
}
#[tokio::test]
async fn provider_rejections_are_secret_free_and_not_outages() {
    for (status, message, outage) in [
        (400, "provider rejected the request (HTTP 400)", false),
        (503, "provider error (HTTP 503)", true),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("secret"))
            .mount(&server)
            .await;
        let q = questions();
        let error = client(&server)
            .decide(
                DecisionRequest {
                    state: "s",
                    questions: &q,
                },
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), message);
        assert_eq!(error.is_outage(), outage);
    }
    assert_eq!(
        DecisionError::ProviderError.to_string(),
        "provider unreachable"
    );
    assert!(DecisionError::ProviderError.is_outage());
}
#[tokio::test]
async fn exact_wire_body_and_usage_defaults_survive_extraction() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/systemone")).and(body_json(serde_json::json!({"model":"model","state":"state","questions":{"go":{"type":"noul","instructions":"Go?"}}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"answers":{"go":{"type":"noul","noul":0.8}},"usage":{},"extra":"tolerated"}))).expect(1).mount(&server).await;
    let questions = questions();
    let response = client(&server)
        .decide(
            DecisionRequest {
                state: "state",
                questions: &questions,
            },
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(response.usage.unwrap().input_tokens, 0);
}
#[tokio::test]
async fn redirects_duplicates_and_oversized_responses_fail_without_retries() {
    for (body, status) in [
        (
            "{\"answers\":{\"go\":{\"type\":\"noul\",\"noul\":0.1,\"noul\":0.9}}}".into(),
            200,
        ),
        ("x".repeat(262145), 200),
        ("".into(), 302),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", format!("{}/elsewhere", server.uri()).as_str())
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&server)
            .await;
        let q = questions();
        let result = client(&server)
            .decide(
                DecisionRequest {
                    state: "s",
                    questions: &q,
                },
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .await;
        assert!(result.is_err());
        assert!(!result.unwrap_err().to_string().contains("secret"));
    }
}
#[tokio::test]
async fn cancellation_and_deadline_cover_send_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(serde_json::json!({"answers":{"go":{"type":"noul","noul":0.8}}})),
        )
        .mount(&server)
        .await;
    let q = questions();
    let c = client(&server);
    assert!(matches!(
        c.decide(
            DecisionRequest {
                state: "s",
                questions: &q
            },
            &CancellationToken::new(),
            Instant::now() + Duration::from_millis(10)
        )
        .await,
        Err(DecisionError::Timeout)
    ));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        c.decide(
            DecisionRequest {
                state: "s",
                questions: &q
            },
            &cancel,
            Instant::now() + Duration::from_secs(2)
        )
        .await,
        Err(DecisionError::Cancelled)
    ));
}
async fn failure(status: u16, body: &str) -> DecisionError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    let q = questions();
    client(&server)
        .decide(
            DecisionRequest {
                state: "s",
                questions: &q,
            },
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err()
}
// A wrong model id answered HTTP 400 on every Instinct call for three days
// (2026-11-23..25) and nothing told the Captain: a refusal was neither an
// outage nor anything else a host could act on. The host must be able to
// read the status and the provider's reason apart from an outage, and a
// refusal must never count as one (it fails at once, and the outage window
// would only hide a configuration that has just been fixed).
#[tokio::test]
async fn a_4xx_is_a_rejection_and_not_an_outage() {
    let refused = failure(
        400,
        r#"{"error":{"message":"model decision-router has no systemone API"}}"#,
    )
    .await;
    assert_eq!(
        refused.rejection(),
        Some((400, Some("model decision-router has no systemone API")))
    );
    assert!(!refused.is_outage());

    // A body echoing the key gives the status alone.
    let echoed = failure(401, "secret").await;
    assert_eq!(echoed.rejection(), Some((401, None)));
    assert!(!echoed.is_outage());

    let down = failure(503, "busy").await;
    assert_eq!(down.rejection(), None);
    assert!(down.is_outage());
    for error in [
        DecisionError::Timeout,
        DecisionError::Cancelled,
        DecisionError::ProviderError,
        DecisionError::InvalidAnswer("x".into()),
        DecisionError::Limit,
    ] {
        assert_eq!(error.rejection(), None, "{error}");
    }
}
