use super::*;
use crate::provider::config::{ModelConfig, ProviderConfig, ReasoningHistory};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE: &str = include_str!("../../../tests/fixtures/omni-models.json");

fn resolved(base_url: &str, model: &str, api: ApiKind, window: Option<u64>) -> ResolvedModel {
    ResolvedModel {
        alias: "alias".into(),
        provider_name: "omni".into(),
        provider: ProviderConfig {
            base_url: base_url.into(),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 5_000,
        },
        model: ModelConfig {
            provider: "omni".into(),
            model: model.into(),
            api,
            context_window: window,
            max_input_tokens: None,
            max_output_tokens: None,
            supports_tools: true,
            supports_parallel_tool_calls: None,
            thinking: None,
            reasoning_history: ReasoningHistory::Preserve,
        },
    }
}

async fn serve(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json"))
        .mount(&server)
        .await;
    server
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

async fn resolve(
    dir: &tempfile::TempDir,
    server: &MockServer,
    model: &str,
    api: ApiKind,
    window: Option<u64>,
) -> Result<ResolvedChat, NoContextWindow> {
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let r = resolved(&format!("{}/v1", server.uri()), model, api, window);
    resolve_chat(&catalog, &r, "key", &ClientCache::new()).await
}

#[tokio::test]
async fn exact_id_match_including_prefixed_ids() {
    let models = parse_models(FIXTURE.as_bytes()).unwrap();
    assert_eq!(models["cc/claude-opus-5-5"].context_length, Some(1_000_000));
    assert_eq!(models["cc/claude-opus-5-5"].max_input_tokens, Some(900_000));
    assert_eq!(
        models["cc/claude-opus-5-5"].max_output_tokens,
        Some(128_000)
    );
    assert!(
        !models.contains_key("claude-opus-5-5"),
        "no suffix matching"
    );
    assert_eq!(
        models["opencode-go/deepseek-v4-flash"].context_length,
        Some(1_000_000)
    );
    assert_eq!(
        models["combo/deepseek-v4-flash"].context_length,
        Some(1_050_000)
    );
}

#[tokio::test]
async fn opus_resolves_to_one_million_without_override() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let r = resolve(&dir, &server, "cc/claude-opus-5-5", ApiKind::Chat, None)
        .await
        .unwrap();
    assert_eq!(r.window, 1_000_000);
    assert_eq!(r.window_source, "discovered.context_length");
    assert_eq!(r.max_input_tokens, Some(900_000));
    assert_eq!(r.reported_output, Some(128_000));
    assert_eq!(r.discrepancy, None);
}

#[tokio::test]
async fn explicit_window_wins_and_exposes_the_discrepancy() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    // Populate the cache, as a first session without an override would.
    resolve(&dir, &server, "cc/claude-opus-5-5", ApiKind::Chat, None)
        .await
        .unwrap();
    let r = resolve(
        &dir,
        &server,
        "cc/claude-opus-5-5",
        ApiKind::Chat,
        Some(200_000),
    )
    .await
    .unwrap();
    assert_eq!(r.window, 200_000);
    assert_eq!(r.window_source, "models.json");
    assert_eq!(
        r.discrepancy,
        Some(Discrepancy {
            configured: 200_000,
            discovered: 1_000_000
        })
    );
    assert_eq!(
        r.discrepancy.unwrap().to_string(),
        "models.json context_window 200000 differs from discovered context_length 1000000"
    );
}

#[tokio::test]
async fn explicit_window_equal_to_the_reported_one_is_no_discrepancy() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    resolve(&dir, &server, "cc/claude-opus-5-5", ApiKind::Chat, None)
        .await
        .unwrap();
    let r = resolve(
        &dir,
        &server,
        "cc/claude-opus-5-5",
        ApiKind::Chat,
        Some(1_000_000),
    )
    .await
    .unwrap();
    assert_eq!(r.discrepancy, None);
}

#[tokio::test]
async fn explicit_window_makes_no_network_call() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let r = resolve(
        &dir,
        &server,
        "cc/claude-opus-5-5",
        ApiKind::Chat,
        Some(200_000),
    )
    .await
    .unwrap();
    assert_eq!(r.window, 200_000);
    assert_eq!(r.discrepancy, None, "nothing cached, nothing to compare");
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn max_input_tokens_is_the_labelled_fallback() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let r = resolve(&dir, &server, "typesafe/jev-1.13", ApiKind::Chat, None)
        .await
        .unwrap();
    assert_eq!(r.window, 64_000);
    assert_eq!(r.window_source, "discovered.max_input_tokens (fallback)");
}

#[tokio::test]
async fn null_zero_and_missing_limits_are_not_a_window() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    for model in [
        "mercury-edit-2",  // context_length null, no input limit
        "missing-limits",  // no limit keys
        "zero-limits",     // all zero
        "not-in-the-list", // unknown id
    ] {
        let err = resolve(&dir, &server, model, ApiKind::Chat, None)
            .await
            .unwrap_err();
        assert_eq!(err.model, model);
    }
    let models = parse_models(FIXTURE.as_bytes()).unwrap();
    let zero = &models["zero-limits"];
    assert_eq!(
        (
            zero.context_length,
            zero.max_input_tokens,
            zero.max_output_tokens
        ),
        (None, None, None),
        "zero is stored as not reported"
    );
    // An explicit zero is not a window either.
    let err = resolve(&dir, &server, "missing-limits", ApiKind::Chat, Some(0))
        .await
        .unwrap_err();
    assert_eq!(err.alias, "alias");
}

#[test]
fn conflicting_duplicate_ids_are_ambiguous_and_dropped() {
    let body = br#"{"data":[
        {"id":"dup","context_length":100000},
        {"id":"ok","context_length":5000},
        {"id":"dup","context_length":200000},
        {"id":"dup","context_length":100000}
    ]}"#;
    let models = parse_models(body).unwrap();
    assert!(!models.contains_key("dup"), "ambiguous ids are not guessed");
    assert_eq!(models["ok"].context_length, Some(5_000));
}

#[test]
fn agreeing_or_complementary_duplicates_are_merged() {
    let body = br#"{"data":[
        {"id":"a","context_length":100000,"max_output_tokens":null},
        {"id":"a","context_length":100000,"max_output_tokens":4096},
        {"id":"b","context_length":null,"max_input_tokens":8000},
        {"id":"b","context_length":16000}
    ]}"#;
    let models = parse_models(body).unwrap();
    assert_eq!(models["a"].context_length, Some(100_000));
    assert_eq!(models["a"].max_output_tokens, Some(4_096));
    assert_eq!(models["b"].context_length, Some(16_000));
    assert_eq!(models["b"].max_input_tokens, Some(8_000));
}

#[test]
fn a_body_that_is_not_a_model_list_is_rejected() {
    assert!(parse_models(b"not json").is_err());
    assert!(parse_models(br#"{"models":[]}"#).is_err());
}

#[tokio::test]
async fn fim_and_systemone_never_look_the_catalog_up() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    for api in [ApiKind::Fim, ApiKind::SystemOne] {
        let r = resolve(&dir, &server, "mercury-edit-2", api, None)
            .await
            .unwrap();
        assert_eq!(r.window_source, "non-chat");
        let r = resolve(&dir, &server, "cc/claude-opus-5-5", api, Some(64_000))
            .await
            .unwrap();
        assert_eq!(r.window, 64_000);
    }
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn the_list_is_fetched_once_and_then_served_from_the_cache() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    for model in ["cc/claude-opus-5-5", "typesafe/jev-1.13"] {
        resolve(&dir, &server, model, ApiKind::Chat, None)
            .await
            .unwrap();
    }
    assert_eq!(requests(&server).await, 1);
}
