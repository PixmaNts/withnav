use super::*;
use crate::provider::config::{ModelConfig, ProviderConfig, ReasoningHistory};
use std::time::Instant;
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

// ---- fetch hardening ----

const OPUS: &str = "cc/claude-opus-5-5";

fn cache_path(catalog: &ModelCatalog, base: &str) -> PathBuf {
    catalog.file(&endpoint_key(base)).expect("data dir")
}

/// Writes a cache file for `base` fetched `age` seconds ago.
fn seed(catalog: &ModelCatalog, base: &str, age: u64, id: &str, limits: Limits) {
    let path = cache_path(catalog, base);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = CacheFile {
        version: 1,
        fetched_at: now() - age,
        models: HashMap::from([(id.to_string(), limits)]),
        carried: HashMap::new(),
    };
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

fn limits(context: u64) -> Limits {
    Limits {
        context_length: Some(context),
        max_input_tokens: None,
        max_output_tokens: None,
    }
}

async fn lookup(catalog: &ModelCatalog, base: &str, model: &str) -> CatalogData {
    let r = resolved(base, model, ApiKind::Chat, None);
    catalog.lookup(&r, "key", &ClientCache::new()).await
}

#[tokio::test]
async fn concurrent_resolutions_make_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(FIXTURE.to_string(), "application/json")
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    let calls = (0..8).map(|_| lookup(&catalog, &base, OPUS));
    let all = futures_util::future::join_all(calls).await;
    assert!(all
        .iter()
        .all(|d| d.limits.as_ref().and_then(|l| l.context_length) == Some(1_000_000)));
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn concurrent_resolutions_share_one_failed_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_millis(200)))
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    let calls = (0..5).map(|_| lookup(&catalog, &base, OPUS));
    let all = futures_util::future::join_all(calls).await;
    assert!(all.iter().all(|d| d.limits.is_none() && d.error.is_some()));
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_cache_past_the_ttl_is_refreshed() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    seed(&catalog, &base, TTL + 10, OPUS, limits(200_000));
    let d = lookup(&catalog, &base, OPUS).await;
    assert_eq!(d.limits.unwrap().context_length, Some(1_000_000));
    assert!(!d.stale);
    assert_eq!(requests(&server).await, 1);
    // The refreshed file is fresh: no second request.
    lookup(&catalog, &base, OPUS).await;
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_cache_inside_the_ttl_is_not_refreshed() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    seed(&catalog, &base, TTL - 100, OPUS, limits(200_000));
    let d = lookup(&catalog, &base, OPUS).await;
    assert_eq!(d.limits.unwrap().context_length, Some(200_000));
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn failure_after_expiry_keeps_the_values_stale_with_age_and_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    seed(&catalog, &base, TTL + 600, OPUS, limits(400_000));
    let r = resolved(&base, OPUS, ApiKind::Chat, None);
    let chat = resolve_chat(&catalog, &r, "key", &ClientCache::new())
        .await
        .unwrap();
    assert_eq!(chat.window, 400_000);
    assert!(chat.stale);
    assert!(chat.stale_age_secs.unwrap() >= TTL + 600);
    let error = chat.refresh_error.unwrap();
    assert!(error.contains("503"), "{error}");
    // The old file is kept as it was.
    assert!(read_cache(&cache_path(&catalog, &base)).is_some());
}

#[tokio::test]
async fn a_failure_with_nothing_cached_has_no_values() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let d = lookup(&catalog, &format!("{}/v1", server.uri()), OPUS).await;
    assert!(d.limits.is_none() && !d.stale && d.error.is_some());
}

#[tokio::test]
async fn an_oversized_body_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let padding = "x".repeat(2_100_000);
    let body = format!(r#"{{"data":[{{"id":"{OPUS}","context_length":5,"note":"{padding}"}}]}}"#);
    let server = serve(&body).await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let d = lookup(&catalog, &format!("{}/v1", server.uri()), OPUS).await;
    assert!(d.limits.is_none());
    assert!(d.error.unwrap().contains("exceeds 2 MB"));
}

#[tokio::test]
async fn a_list_with_too_many_entries_is_rejected() {
    let entries = (0..=MAX_ENTRIES)
        .map(|i| format!(r#"{{"id":"m{i}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_models(format!(r#"{{"data":[{entries}]}}"#).as_bytes()).is_err());
}

#[tokio::test]
async fn a_slow_endpoint_hits_the_deadline_and_keeps_stale_values() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    catalog.deadline = Duration::from_millis(200);
    let base = format!("{}/v1", server.uri());
    seed(&catalog, &base, TTL + 1, OPUS, limits(300_000));
    let started = Instant::now();
    let d = lookup(&catalog, &base, OPUS).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(d.stale);
    assert_eq!(d.limits.unwrap().context_length, Some(300_000));
    assert!(d.error.unwrap().contains("deadline"));
}

#[tokio::test]
async fn a_cross_origin_redirect_is_not_followed_with_the_bearer() {
    let dir = tempfile::tempdir().unwrap();
    let target = serve(FIXTURE).await;
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/v1/models", target.uri()).as_str()),
        )
        .mount(&origin)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", origin.uri());
    let d = lookup(&catalog, &base, OPUS).await;
    assert!(d.limits.is_none());
    assert!(d.error.unwrap().contains("another origin"));
    assert_eq!(
        requests(&origin).await,
        1,
        "the origin was asked, with the key"
    );
    assert_eq!(
        requests(&target).await,
        0,
        "the redirect target was never contacted"
    );
}

#[tokio::test]
async fn a_same_origin_redirect_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/v1/list"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(FIXTURE.to_string(), "application/json"),
        )
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let d = lookup(&catalog, &format!("{}/v1", server.uri()), OPUS).await;
    assert_eq!(d.limits.unwrap().context_length, Some(1_000_000));
}

#[tokio::test]
async fn a_corrupt_cache_file_is_ignored_and_rewritten() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    let file = cache_path(&catalog, &base);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"{ not json").unwrap();
    let d = lookup(&catalog, &base, OPUS).await;
    assert_eq!(d.limits.unwrap().context_length, Some(1_000_000));
    assert_eq!(requests(&server).await, 1);
    let rewritten = read_cache(&file).expect("valid file after the refresh");
    assert_eq!(rewritten.version, 1);
}

#[tokio::test]
async fn an_unwritable_cache_dir_degrades_to_memory_with_one_warning() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    // The data dir is a regular file: no directory can be created in it.
    let blocker = dir.path().join("data");
    std::fs::write(&blocker, b"x").unwrap();
    let catalog = ModelCatalog::new(Some(blocker));
    let base = format!("{}/v1", server.uri());
    let first = lookup(&catalog, &base, OPUS).await;
    assert_eq!(first.limits.unwrap().context_length, Some(1_000_000));
    // Served from memory: no second request.
    let second = lookup(&catalog, &base, "typesafe/jev-1.13").await;
    assert_eq!(second.limits.unwrap().max_input_tokens, Some(64_000));
    assert_eq!(requests(&server).await, 1);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

#[test]
fn an_unwritable_cache_dir_warns_once() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("data");
    std::fs::write(&blocker, b"x").unwrap();
    let catalog = ModelCatalog::new(Some(blocker));
    let target = dir.path().join("data").join("model-catalog").join("x.json");
    let file = CacheFile {
        version: 1,
        fetched_at: now(),
        models: HashMap::new(),
        carried: HashMap::new(),
    };
    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..3 {
            catalog.persist(&target, &file);
        }
    });
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        text.matches("cannot write the model catalog cache").count(),
        1
    );
}

#[tokio::test]
async fn the_cache_file_holds_no_key_or_header() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    let mut r = resolved(&base, OPUS, ApiKind::Chat, None);
    r.provider
        .headers
        .insert("x-private".into(), "header-secret-value".into());
    catalog
        .lookup(&r, "sk-secret-key-123", &ClientCache::new())
        .await;
    let sent = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        sent.headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer sk-secret-key-123",
        "the key is sent..."
    );
    let raw = std::fs::read_to_string(cache_path(&catalog, &base)).unwrap();
    assert!(!raw.contains("sk-secret-key-123"), "...but never stored");
    assert!(!raw.contains("header-secret-value"));
    assert!(!raw.to_lowercase().contains("authorization"));
    let names = std::fs::read_dir(cache_path(&catalog, &base).parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(names, 1, "no temporary file is left behind");
}

#[tokio::test]
async fn null_or_malformed_fields_keep_the_previous_values() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"{"data":[{"id":"m","context_length":null,"max_input_tokens":"lots","max_output_tokens":4096}]}"#;
    let server = serve(body).await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    seed(
        &catalog,
        &base,
        TTL + 1,
        "m",
        Limits {
            context_length: Some(500_000),
            max_input_tokens: Some(400_000),
            max_output_tokens: Some(1_000),
        },
    );
    let d = lookup(&catalog, &base, "m").await;
    let l = d.limits.unwrap();
    assert_eq!(l.context_length, Some(500_000), "null keeps the old value");
    assert_eq!(l.max_input_tokens, Some(400_000), "malformed keeps it too");
    assert_eq!(
        l.max_output_tokens,
        Some(4_096),
        "a valid new value replaces"
    );
    assert_eq!(requests(&server).await, 1);
    assert!(d.stale, "kept values are not freshly confirmed");
    assert!(d.age_seconds.unwrap() >= TTL + 1, "{:?}", d.age_seconds);
    let saved = read_cache(&cache_path(&catalog, &base)).unwrap();
    assert_eq!(saved.models["m"].context_length, Some(500_000));
    // The same after a restart: no new request, still stale with the old age.
    let reopened = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let again = lookup(&reopened, &base, "m").await;
    assert_eq!(requests(&server).await, 1, "the list itself is current");
    assert!(again.stale);
    assert!(again.age_seconds.unwrap() >= TTL + 1);
    let cached = reopened.cached(&resolved(&base, "m", ApiKind::Chat, None));
    assert!(cached.stale && cached.age_seconds.unwrap() >= TTL + 1);
    assert_eq!(cached.limits.unwrap().context_length, Some(500_000));
}

#[tokio::test]
async fn a_cache_file_with_a_zero_limit_is_ignored_and_refreshed() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    // Fresh and valid JSON, but with a window of zero.
    seed(&catalog, &base, 0, OPUS, limits(0));
    assert!(read_cache(&cache_path(&catalog, &base)).is_none());
    assert!(catalog
        .cached(&resolved(&base, OPUS, ApiKind::Chat, None))
        .limits
        .is_none());
    let d = lookup(&catalog, &base, OPUS).await;
    assert_eq!(d.limits.unwrap().context_length, Some(1_000_000));
    assert_eq!(requests(&server).await, 1, "the damaged file is refreshed");
    let rewritten = read_cache(&cache_path(&catalog, &base)).expect("valid after the refresh");
    assert_eq!(rewritten.models[OPUS].context_length, Some(1_000_000));
}

#[test]
fn a_saved_resolution_needs_a_positive_window() {
    let good = ModelResolution {
        version: RESOLUTION_VERSION,
        provider: "p".into(),
        model: "m".into(),
        api: "chat".into(),
        endpoint: "http://h/v1".into(),
        bounds: Bounds {
            context_window: 1,
            max_input_tokens: Some(1),
            reported_output: None,
        },
        history_policy: "preserve".into(),
        provenance: Provenance {
            context_window: "models.json".into(),
            max_input_tokens: "none".into(),
            reported_output: "none".into(),
        },
    };
    let raw = |r: &ModelResolution| serde_json::to_string(r).unwrap();
    assert!(ModelResolution::from_saved(&raw(&good)).is_ok());
    let mut zero_window = good.clone();
    zero_window.bounds.context_window = 0;
    assert!(ModelResolution::from_saved(&raw(&zero_window))
        .unwrap_err()
        .contains("context_window"));
    let mut zero_input = good;
    zero_input.bounds.max_input_tokens = Some(0);
    assert!(ModelResolution::from_saved(&raw(&zero_input))
        .unwrap_err()
        .contains("max_input_tokens"));
}
