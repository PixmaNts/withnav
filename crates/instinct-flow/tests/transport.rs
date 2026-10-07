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
