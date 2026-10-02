//! The model resolution saved with a conversation (`sessions.model_resolution`):
//! written at creation and at the first activation of a legacy row, frozen
//! while the session lives, served on resume when discovery is down, and
//! never moved to another model by a retargeted alias.

use navai::error::ServerError;
use navai::session::Service;
use serde_json::{json, Value};
use ulid::Ulid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::server::TestServerConfig;

fn models_body(window: u64) -> Value {
    json!({"data": [{
        "id": "mock-model",
        "context_length": window,
        "max_input_tokens": 900_000,
        "max_output_tokens": 128_000
    }]})
}

async fn serve_models(window: u64) -> MockServer {
    let server = MockServer::start().await;
    mount_models(&server, window).await;
    server
}

async fn mount_models(server: &MockServer, window: u64) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_body(window)))
        .mount(server)
        .await;
}

struct Fixture {
    tmp: tempfile::TempDir,
    ws: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        Self { tmp, ws }
    }

    /// A service over an alias with NO window of its own: discovery is the
    /// only source of its bounds. `data` names the catalog cache directory.
    fn service(&self, base_url: &str, model: &str, data: &str) -> Service {
        let cfg = TestServerConfig::mock(self.tmp.path(), base_url)
            .model_alias("default", model)
            .without_window();
        let (mut session, _) = cfg.config().split();
        session.data_dir = self.tmp.path().join(data);
        Service::new(session).expect("service")
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.tmp.path().join("history.db")).expect("db")
    }

    fn saved(&self, id: Ulid) -> Option<String> {
        self.db()
            .query_row(
                "SELECT model_resolution FROM sessions WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .expect("row")
    }
}

fn base(server: &MockServer) -> String {
    format!("{}/v1", server.uri())
}

async fn create_then_stop(service: &Service, fx: &Fixture) -> Ulid {
    let id = service
        .create_session(&fx.ws, None, None)
        .await
        .expect("create");
    service.close_session(id).await.expect("close");
    service.history().flush().await.expect("flush");
    id
}

// Given a model whose window only the provider reports
// When a session is created
// Then its row holds the versioned resolution: identity, endpoint without
//      credentials, bounds and where each came from
#[tokio::test]
async fn creation_saves_the_resolution() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let service = fx.service(&base(&server), "mock-model", "data");
    let id = create_then_stop(&service, &fx).await;

    let saved: Value = serde_json::from_str(&fx.saved(id).expect("saved")).expect("json");
    assert_eq!(
        saved,
        json!({
            "version": 1,
            "provider": "mock",
            "model": "mock-model",
            "api": "chat",
            "endpoint": base(&server),
            "bounds": {
                "context_window": 1_000_000,
                "max_input_tokens": 900_000,
                "reported_output": 128_000
            },
            "history_policy": "preserve",
            "provenance": {
                "context_window": "discovered.context_length",
                "max_input_tokens": "discovered.max_input_tokens",
                "reported_output": "discovered.max_output_tokens"
            }
        })
    );
}

// Given a conversation with a saved resolution
// When it is resumed with the provider down and a cold catalog cache
// Then it resumes on the saved bound, and the row is unchanged
#[tokio::test]
async fn resume_with_discovery_down_uses_the_saved_bound() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let url = base(&server);
    let id = create_then_stop(&fx.service(&url, "mock-model", "data"), &fx).await;
    let before = fx.saved(id);
    drop(server);

    // A new service with its own empty cache, nothing listening.
    let cold = fx.service(&url, "mock-model", "cold-data");
    cold.create_or_resume_session(&fx.ws, None, None, Some(id))
        .await
        .expect("resume on the saved bound");
    assert_eq!(fx.saved(id), before, "the saved bound is not rewritten");
    cold.close_session(id).await.expect("close");
}

// Given a saved resolution on one model
// When the alias now maps to another model
// Then the resume is the identity error naming both, and nothing is rewritten
#[tokio::test]
async fn a_retargeted_alias_is_an_identity_error() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let url = base(&server);
    let id = create_then_stop(&fx.service(&url, "mock-model", "data"), &fx).await;
    let before = fx.saved(id);

    let moved = fx.service(&url, "other-model", "data");
    let err = moved
        .create_or_resume_session(&fx.ws, None, None, Some(id))
        .await
        .expect_err("a retargeted alias never migrates a conversation");
    assert!(
        matches!(err, ServerError::ModelIdentityChanged { .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("provider 'mock', model 'mock-model', api chat"),
        "{text}"
    );
    assert!(
        text.contains("provider 'mock', model 'other-model', api chat"),
        "{text}"
    );
    assert!(text.contains("alias 'default'"), "{text}");
    assert_eq!(fx.saved(id), before);
}

// Given a row from before the column
// When it is first resumed
// Then it gets its snapshot then, and keeps it when the provider later
//      reports something else; with no window from any source it is refused
#[tokio::test]
async fn a_legacy_row_gets_its_snapshot_at_activation() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let url = base(&server);

    // Refused first: no saved bound, provider down, cold cache.
    let down = MockServer::start().await;
    let down_url = base(&down);
    drop(down);
    let legacy = Ulid::new();
    let service = fx.service(&down_url, "mock-model", "cold-data");
    service
        .history()
        .record_session(legacy, &fx.ws.display().to_string(), "default", None)
        .await
        .expect("legacy row");
    assert_eq!(fx.saved(legacy), None);
    let err = service
        .create_or_resume_session(&fx.ws, None, None, Some(legacy))
        .await
        .expect_err("no window from any source");
    assert!(
        matches!(err, ServerError::NoContextWindow { .. }),
        "{err:?}"
    );
    assert_eq!(fx.saved(legacy), None, "a refusal saves nothing");

    // Then resolved with the provider up.
    let service = fx.service(&url, "mock-model", "data");
    service
        .create_or_resume_session(&fx.ws, None, None, Some(legacy))
        .await
        .expect("resume");
    service.close_session(legacy).await.expect("close");
    service.history().flush().await.expect("flush");
    let saved: Value = serde_json::from_str(&fx.saved(legacy).expect("snapshot")).unwrap();
    assert_eq!(saved["bounds"]["context_window"], 1_000_000);

    // The next resume keeps it, whatever the provider says now.
    mount_models(&server, 500_000).await;
    let fresh = fx.service(&url, "mock-model", "other-cache");
    fresh
        .create_or_resume_session(&fx.ws, None, None, Some(legacy))
        .await
        .expect("resume again");
    fresh.close_session(legacy).await.expect("close");
    fresh.history().flush().await.expect("flush");
    let again: Value = serde_json::from_str(&fx.saved(legacy).expect("snapshot")).unwrap();
    assert_eq!(again, saved);
}

// Given a live session
// When the catalog is refreshed and now reports another window
// Then the session's saved bound does not change, on this run or the next
#[tokio::test]
async fn a_catalog_refresh_does_not_change_a_live_session() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let url = base(&server);
    let service = fx.service(&url, "mock-model", "data");
    let id = service
        .create_session(&fx.ws, None, None)
        .await
        .expect("create");
    service.history().flush().await.expect("flush");
    let before = fx.saved(id);

    mount_models(&server, 500_000).await;
    std::fs::remove_dir_all(fx.tmp.path().join("data/model-catalog")).expect("expire cache");
    let refreshed = service
        .resolve_model_metadata(&fx.ws, "default")
        .await
        .expect("refresh");
    assert_eq!(refreshed.window, 500_000, "the catalog did refresh");
    assert_eq!(fx.saved(id), before, "the live session keeps its bound");

    service.close_session(id).await.expect("close");
    service
        .create_or_resume_session(&fx.ws, None, None, Some(id))
        .await
        .expect("resume");
    service.close_session(id).await.expect("close");
    service.history().flush().await.expect("flush");
    assert_eq!(fx.saved(id), before, "nor does the next activation");
}

// Given a row whose column is not valid resolution JSON
// When it is resumed
// Then the resume fails clearly, naming the session, instead of resolving anew
#[tokio::test]
async fn a_corrupt_column_fails_the_resume() {
    let fx = Fixture::new();
    let server = serve_models(1_000_000).await;
    let service = fx.service(&base(&server), "mock-model", "data");
    let id = create_then_stop(&service, &fx).await;

    for bad in [
        "{not json".to_string(),
        r#"{"version":1,"provider":"mock"}"#.to_string(),
        fx.saved(id)
            .unwrap()
            .replace("\"version\":1", "\"version\":9"),
        // Valid JSON of the right shape, but a window of zero.
        fx.saved(id)
            .unwrap()
            .replace("\"context_window\":1000000", "\"context_window\":0"),
    ] {
        fx.db()
            .execute(
                "UPDATE sessions SET model_resolution = ?2 WHERE id = ?1",
                rusqlite::params![id.to_string(), bad],
            )
            .expect("corrupt");
        let err = service
            .create_or_resume_session(&fx.ws, None, None, Some(id))
            .await
            .expect_err("a corrupt column is not ignored");
        assert!(
            matches!(err, ServerError::SavedResolutionInvalid { .. }),
            "{bad}: {err:?}"
        );
        assert!(err.to_string().contains(&id.to_string()), "{err}");
        assert_eq!(fx.saved(id).as_deref(), Some(bad.as_str()), "left as found");
    }
}
