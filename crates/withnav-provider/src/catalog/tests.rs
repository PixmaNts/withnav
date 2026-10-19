use super::*;

#[tokio::test]
async fn learned_message_limit_is_monotone_and_survives_refresh_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = ModelCatalog::new(Some(dir.path().into()));
    catalog
        .learn_message_limit("http://localhost/v1", "m", 128000)
        .await;
    catalog
        .learn_message_limit("http://localhost/v1", "m", 200000)
        .await;
    let restarted = ModelCatalog::new(Some(dir.path().into()));
    assert_eq!(
        restarted.message_limit("http://localhost/v1", "m"),
        Some(128000)
    );
    let key = endpoint_key("http://localhost/v1");
    let cache = catalog
        .current(&catalog.endpoint(&key), catalog.file(&key).as_deref())
        .unwrap();
    let (merged, _) = merge(
        Some(&cache),
        HashMap::from([(
            "m".into(),
            Limits {
                context_length: Some(1000000),
                ..Default::default()
            },
        )]),
    );
    assert_eq!(merged["m"].max_message_tokens, Some(128000));
}

#[test]
fn optional_configured_message_limit_rejects_zero_and_keeps_legacy_models() {
    let model = |field: &str| format!(r#"{{"provider":"p","model":"m"{field}}}"#);
    let legacy: ModelConfig = serde_json::from_str(&model("")).unwrap();
    assert_eq!(legacy.max_message_tokens, None);
    let configured: ModelConfig =
        serde_json::from_str(&model(",\"max_message_tokens\":12000")).unwrap();
    assert_eq!(configured.max_message_tokens, Some(12000));
    assert!(serde_json::from_str::<ModelConfig>(&model(",\"max_message_tokens\":0")).is_err());
}

#[test]
fn oversized_cache_is_ignored() {
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len(100 * 1024 * 1024).unwrap();
    assert!(read_cache(file.path()).is_none());
}
use crate::config::{ModelConfig, ProviderConfig, ReasoningHistory};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE: &str = include_str!("../../../../tests/fixtures/omni-models.json");

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
            catalog_format: None,
        },
        model: ModelConfig {
            provider: "omni".into(),
            model: model.into(),
            api,
            context_window: window,
            max_input_tokens: None,
            max_message_tokens: None,
            max_output_tokens: None,
            supports_tools: true,
            supports_parallel_tool_calls: None,
            images: None,
            thinking: None,
            default_variant: None,
            variants: None,
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
    server
        .received_requests()
        .await
        .expect("inspect mock requests")
        .len()
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

async fn resolve_fixture(model: &str, window: Option<u64>) -> (ResolvedChat, MockServer) {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(FIXTURE).await;
    let chat = resolve(&dir, &server, model, ApiKind::Chat, window)
        .await
        .unwrap();
    (chat, server)
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
    let (r, _server) = resolve_fixture("cc/claude-opus-5-5", None).await;
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
    for (window, discrepancy) in [
        (
            200_000,
            Some(Discrepancy {
                configured: 200_000,
                discovered: 1_000_000,
            }),
        ),
        (1_000_000, None),
    ] {
        let r = resolve(
            &dir,
            &server,
            "cc/claude-opus-5-5",
            ApiKind::Chat,
            Some(window),
        )
        .await
        .unwrap();
        assert_eq!(r.window, window, "configured_window={window}");
        assert_eq!(r.window_source, "models.json", "configured_window={window}");
        assert_eq!(r.discrepancy, discrepancy, "configured_window={window}");
        if let Some(d) = r.discrepancy {
            assert_eq!(
                d.to_string(),
                "models.json context_window 200000 differs from discovered context_length 1000000"
            );
        }
    }
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn explicit_window_makes_no_network_call() {
    let (r, server) = resolve_fixture("cc/claude-opus-5-5", Some(200_000)).await;
    assert_eq!(r.window, 200_000);
    assert_eq!(r.discrepancy, None, "nothing cached, nothing to compare");
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn max_input_tokens_is_the_labelled_fallback() {
    let (r, _server) = resolve_fixture("typesafe/jev-1.13", None).await;
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
            .unwrap_or_else(|e| panic!("api={api:?}: {e:?}"));
        assert_eq!(r.window_source, "non-chat", "api={api:?}");
        let r = resolve(&dir, &server, "cc/claude-opus-5-5", api, Some(64_000))
            .await
            .unwrap_or_else(|e| panic!("api={api:?}: {e:?}"));
        assert_eq!(r.window, 64_000, "api={api:?}");
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
        version: CACHE_VERSION,
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
        effort_tiers: None,
        max_message_tokens: None,
    }
}

async fn lookup(catalog: &ModelCatalog, base: &str, model: &str) -> CatalogData {
    let r = resolved(base, model, ApiKind::Chat, None);
    catalog.lookup(&r, "key", &ClientCache::new()).await
}

async fn concurrent_lookup_response(
    status: u16,
    body: &'static str,
    callers: usize,
) -> Vec<CatalogData> {
    let dir = tempfile::tempdir().unwrap();
    let received = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let router = axum::Router::new().route(
        "/v1/models",
        axum::routing::get({
            let received = received.clone();
            let release = release.clone();
            let count = count.clone();
            move || {
                let received = received.clone();
                let release = release.clone();
                let count = count.clone();
                async move {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        received.notify_one();
                        tokio::time::timeout(Duration::from_secs(15), release.notified())
                            .await
                            .expect("release catalogue response");
                    }
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .expect("serve gated catalogue response");
    });
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let all = tokio::time::timeout(Duration::from_secs(15), async {
        let mut first = Box::pin(lookup(&catalog, &base, OPUS));
        tokio::select! {
            _ = received.notified() => {},
            _ = &mut first => panic!("first lookup completed before response release"),
        }
        let endpoint = catalog.endpoint(&endpoint_key(&base));
        assert!(
            endpoint.flight.try_lock().is_err(),
            "first lookup holds the flight"
        );
        let mut waiting: Vec<_> = (1..callers)
            .map(|_| Box::pin(lookup(&catalog, &base, OPUS)))
            .collect();
        // Poll each caller while the response is held: all must queue on the flight lock.
        std::future::poll_fn(|cx| {
            for call in &mut waiting {
                assert!(
                    std::future::Future::poll(call.as_mut(), cx).is_pending(),
                    "caller bypassed the in-flight request"
                );
            }
            std::task::Poll::Ready(())
        })
        .await;
        release.notify_one();
        let mut all = vec![first.await];
        all.extend(futures_util::future::join_all(waiting).await);
        all
    })
    .await
    .expect("concurrent catalogue lookups must complete");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("catalogue server must stop")
        .expect("catalogue server must not panic");
    all
}

#[tokio::test]
async fn concurrent_resolutions_make_one_request() {
    let all = concurrent_lookup_response(200, FIXTURE, 8).await;
    assert!(
        all.iter()
            .all(|d| d.limits.as_ref().and_then(|l| l.context_length) == Some(1_000_000))
    );
}

#[tokio::test]
async fn concurrent_resolutions_share_one_failed_request() {
    let all = concurrent_lookup_response(500, "", 5).await;
    assert!(all.iter().all(|d| d.limits.is_none() && d.error.is_some()));
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
async fn a_version_1_cache_stays_usable_while_the_refresh_fails() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    // A young file written before the tiers were kept.
    let path = cache_path(&catalog, &base);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let v1 = format!(
        r#"{{"version":1,"fetched_at":{},"models":{{"{OPUS}":{{"context_length":400000,"max_input_tokens":null,"max_output_tokens":null}}}}}}"#,
        now() - 60
    );
    std::fs::write(&path, v1).unwrap();
    let r = resolved(&base, OPUS, ApiKind::Chat, None);
    let chat = resolve_chat(&catalog, &r, "key", &ClientCache::new())
        .await
        .unwrap();
    assert_eq!(chat.window, 400_000, "the known window is kept");
    assert!(chat.stale, "a file whose refresh failed is stale");
    assert!(chat.refresh_error.unwrap().contains("503"));
    assert_eq!(
        requests(&server).await,
        1,
        "the tiers are asked for at once"
    );
    assert!(read_cache(&path).is_some(), "the old file is not discarded");
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
    let d = tokio::time::timeout(Duration::from_secs(15), lookup(&catalog, &base, OPUS))
        .await
        .expect("catalogue deadline must bound the lookup");
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
    assert_eq!(rewritten.version, CACHE_VERSION);
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
        version: CACHE_VERSION,
        fetched_at: now(),
        models: HashMap::new(),
        carried: HashMap::new(),
    };
    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    // In October 2026, a parallel first warning cached no interest on its thread before this
    // thread installed its subscriber. Exercise that ordering, then register our interest again.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let other = ModelCatalog::new(None);
                other.persist(&target, &file);
            })
            .join()
            .unwrap();
    });
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::callsite::rebuild_interest_cache();
        for _ in 0..3 {
            assert!(!catalog.persist(&target, &file));
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
    let requests = server
        .received_requests()
        .await
        .expect("inspect mock requests");
    let sent = requests.first().expect("catalogue request was received");
    assert_eq!(
        sent.headers
            .get("authorization")
            .expect("catalogue request has Authorization header")
            .to_str()
            .expect("Authorization header is valid text"),
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
            effort_tiers: None,
            max_message_tokens: None,
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
    assert!(d.age_seconds.unwrap() > TTL, "{:?}", d.age_seconds);
    let saved = read_cache(&cache_path(&catalog, &base)).unwrap();
    assert_eq!(saved.models["m"].context_length, Some(500_000));
    // The same after a restart: no new request, still stale with the old age.
    let reopened = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let again = lookup(&reopened, &base, "m").await;
    assert_eq!(requests(&server).await, 1, "the list itself is current");
    assert!(again.stale);
    assert!(again.age_seconds.unwrap() > TTL);
    let cached = reopened.cached(&resolved(&base, "m", ApiKind::Chat, None));
    assert!(cached.stale && cached.age_seconds.unwrap() > TTL);
    assert_eq!(cached.limits.unwrap().context_length, Some(500_000));
}

fn tiers_of(d: &CatalogData) -> Option<Vec<String>> {
    d.limits.as_ref().and_then(|l| l.effort_tiers.clone())
}

/// Seeds an expired entry with tiers `low`, `high`, refreshes it against
/// `body`, then reads it again after a restart.
async fn refreshed_tiers(body: &str) -> (CatalogData, CatalogData) {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(body).await;
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    let mut seeded = limits(500_000);
    seeded.effort_tiers = Some(vec!["low".into(), "high".into()]);
    seed(&catalog, &base, TTL + 1, "m", seeded);
    let first = lookup(&catalog, &base, "m").await;
    assert_eq!(requests(&server).await, 1);
    let reopened = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let again = lookup(&reopened, &base, "m").await;
    assert_eq!(requests(&server).await, 1, "the list itself is current");
    (first, again)
}

#[tokio::test]
async fn tiers_the_provider_did_not_validly_report_are_kept() {
    for capabilities in [
        r#""capabilities":{}"#,
        r#""capabilities":{"effort_tiers":null}"#,
        r#""capabilities":{"effort_tiers":"high"}"#,
        r#""capabilities":null"#,
        r#""capabilities":{"effort_tiers":[null,42]}"#,
        r#""capabilities":{"effort_tiers":["","not a tier"]}"#,
    ] {
        let body = format!(r#"{{"data":[{{"id":"m","context_length":500000,{capabilities}}}]}}"#);
        let (first, again) = refreshed_tiers(&body).await;
        for d in [first, again] {
            assert_eq!(
                tiers_of(&d),
                Some(vec!["low".to_string(), "high".to_string()]),
                "{capabilities}"
            );
            assert!(
                d.stale,
                "kept tiers are not freshly confirmed: {capabilities}"
            );
        }
    }
}

#[tokio::test]
async fn an_explicit_empty_tier_list_removes_the_tiers() {
    let body =
        r#"{"data":[{"id":"m","context_length":500000,"capabilities":{"effort_tiers":[]}}]}"#;
    let (first, again) = refreshed_tiers(body).await;
    for d in [first, again] {
        assert_eq!(tiers_of(&d), Some(Vec::new()));
        assert!(!d.stale);
    }
}

#[tokio::test]
async fn a_generated_default_is_resolved_and_one_the_catalog_dropped_is_reported() {
    let tiers = |t: &str| {
        format!(
            r#"{{"data":[{{"id":"m","context_length":8000,"capabilities":{{"effort_tiers":{t}}}}}]}}"#
        )
    };
    let mut r = resolved("", "m", ApiKind::Chat, None);
    r.provider.catalog_format = Some(crate::config::CatalogFormat::Omniroute);
    r.model.default_variant = Some("max".into());
    for (reported, expected) in [(r#"["low","max"]"#, true), (r#"["low"]"#, false)] {
        let (dir, server) = (tempfile::tempdir().unwrap(), serve(&tiers(reported)).await);
        r.provider.base_url = format!("{}/v1", server.uri());
        let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
        let chat = resolve_chat(&catalog, &r, "key", &ClientCache::new())
            .await
            .unwrap();
        let chosen = chat.default_override("alias");
        if expected {
            let (name, v) = chosen.unwrap().unwrap();
            assert_eq!(name, "max");
            assert_eq!(v["reasoning_effort"], "max");
        } else {
            assert_eq!(chosen.unwrap_err().variant, "max");
        }
    }
}

#[tokio::test]
async fn a_cache_file_with_a_zero_limit_is_ignored_and_refreshed() {
    let (dir, server) = (tempfile::tempdir().unwrap(), serve(FIXTURE).await);
    let catalog = ModelCatalog::new(Some(dir.path().to_path_buf()));
    let base = format!("{}/v1", server.uri());
    // Fresh and valid JSON, but with a window of zero.
    seed(&catalog, &base, 0, OPUS, limits(0));
    assert!(read_cache(&cache_path(&catalog, &base)).is_none());
    assert!(
        catalog
            .cached(&resolved(&base, OPUS, ApiKind::Chat, None))
            .limits
            .is_none()
    );
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
    assert!(
        ModelResolution::from_saved(&raw(&zero_window))
            .unwrap_err()
            .contains("context_window")
    );
    let mut zero_input = good;
    zero_input.bounds.max_input_tokens = Some(0);
    assert!(
        ModelResolution::from_saved(&raw(&zero_input))
            .unwrap_err()
            .contains("max_input_tokens")
    );
}
