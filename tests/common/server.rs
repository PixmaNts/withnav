//! Fixtures for the session-server tests: models and auth files, in-process
//! and real-socket servers, HTTP helpers.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use rust_agent::bus::command::{Command, ContentBlock, DeliveryMode};
use rust_agent::bus::event::EventEnvelope;
use rust_agent::server::{AppState, ServerConfig};

use super::sse::spawn_mock_provider;

/// A provider URL nothing listens on. Creating a session loads the models
/// file but makes no network call, so it is never dialed.
pub const UNREACHABLE_PROVIDER_URL: &str = "http://127.0.0.1:1/v1";

/// Name of the provider in the generated `models.json` and `auth.json`.
const PROVIDER: &str = "mock";

/// Writes `auth.json` under `config_dir` with `api_key` for `provider`,
/// readable by the owner only.
pub fn write_auth_file(config_dir: &Path, provider: &str, api_key: &str) -> PathBuf {
    let path = config_dir.join("auth.json");
    std::fs::write(
        &path,
        json!({ provider: { "api_key": api_key } }).to_string(),
    )
    .expect("write auth.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
    }
    path
}

/// Builds a [`ServerConfig`] rooted in a temp directory, writing the
/// `models.json` and `cfg/auth.json` it refers to. The history database
/// defaults to `<root>/history.db`, so a test server never touches the
/// user's data dir; `root` must be a temp directory that outlives the
/// server.
pub struct TestServerConfig {
    root: PathBuf,
    base_url: String,
    alias: String,
    model: String,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    bearer_token: Option<String>,
    history_db: Option<PathBuf>,
}

impl TestServerConfig {
    /// A provider that is never dialed (short timeouts, dead port).
    pub fn unreachable(root: &Path) -> Self {
        Self {
            connect_timeout_ms: 100,
            request_timeout_ms: 100,
            ..Self::mock(root, UNREACHABLE_PROVIDER_URL)
        }
    }

    /// A provider reachable at `base_url` (a scripted mock).
    pub fn mock(root: &Path, base_url: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            base_url: base_url.to_string(),
            alias: "default".to_string(),
            model: "mock-model".to_string(),
            connect_timeout_ms: 2000,
            request_timeout_ms: 600_000,
            bearer_token: None,
            history_db: None,
        }
    }

    /// Names the single model alias the `models.json` defines.
    pub fn model_alias(mut self, alias: &str, model: &str) -> Self {
        self.alias = alias.to_string();
        self.model = model.to_string();
        self
    }

    pub fn bearer_token(mut self, token: &str) -> Self {
        self.bearer_token = Some(token.to_string());
        self
    }

    pub fn history_db(mut self, path: PathBuf) -> Self {
        self.history_db = Some(path);
        self
    }

    /// Writes the models and auth files and returns the config.
    pub fn config(&self) -> ServerConfig {
        let models_path = self.root.join("models.json");
        std::fs::write(
            &models_path,
            json!({
                "providers": {
                    PROVIDER: {
                        "base_url": self.base_url,
                        "api_key_env": "RUST_AGENT_TEST_KEY",
                        "headers": {},
                        "connect_timeout_ms": self.connect_timeout_ms,
                        "request_timeout_ms": self.request_timeout_ms
                    }
                },
                "models": {
                    self.alias.as_str(): {
                        "provider": PROVIDER,
                        "model": self.model,
                        "reasoning_history": "preserve"
                    }
                }
            })
            .to_string(),
        )
        .expect("write models.json");
        let config_dir = self.root.join("cfg");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        write_auth_file(&config_dir, PROVIDER, "test-key");
        ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            models_path: Some(models_path),
            config_dir,
            allow_non_loopback: false,
            bearer_token: self.bearer_token.clone(),
            history_db: self
                .history_db
                .clone()
                .unwrap_or_else(|| self.root.join("history.db")),
        }
    }

    /// An in-process [`AppState`] over [`Self::config`].
    pub fn state(&self) -> AppState {
        AppState::new(self.config()).expect("state over a temp history database")
    }

    /// Like [`Self::state`], with the navigator profile's hooks.
    pub fn navigator_state(&self) -> AppState {
        self.navigator_state_with_conversation(None)
    }

    /// [`Self::navigator_state`] sharing the navigator's conversation feed.
    pub fn navigator_state_with_conversation(
        &self,
        conversation: Option<rust_agent::navigator::ConversationFeed>,
    ) -> AppState {
        let hooks = match conversation {
            Some(feed) => rust_agent::navigator::NavigatorHooks::with_feed(feed),
            None => rust_agent::navigator::NavigatorHooks::new(),
        };
        AppState::with_hooks(self.config(), std::sync::Arc::new(hooks))
            .expect("state over a temp history database")
    }
}

/// Polls `/v1/health` on `addr` until it answers; panics after 2 s.
pub async fn wait_until_healthy(addr: SocketAddr) {
    for _ in 0..100 {
        if reqwest::get(format!("http://{addr}/v1/health"))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server never came up");
}

/// Serves `state` on an ephemeral loopback socket (the real router) and
/// waits until it answers `/v1/health`. The caller aborts the task.
pub async fn spawn_server(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let _ = rust_agent::server::routes::serve_state_on(state, listener).await;
    });
    wait_until_healthy(addr).await;
    (addr, task)
}

/// A real-socket server over a scripted mock provider, in its own tempdir.
/// Dropping it aborts the server task.
pub struct TestServer {
    pub base: String,
    pub state: AppState,
    pub ws: PathBuf,
    task: tokio::task::JoinHandle<()>,
    _root: tempfile::TempDir,
}

impl TestServer {
    /// Starts a server whose provider answers `mock_responses`, one per
    /// connection.
    pub async fn start(mock_responses: Vec<Vec<u8>>) -> Self {
        let mock = spawn_mock_provider(mock_responses).await;
        Self::start_with_provider_url(&mock.url).await
    }

    /// Starts a server whose provider lives at `provider_url`, for a test
    /// that owns its mock connection directly.
    pub async fn start_with_provider_url(provider_url: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let state = TestServerConfig::mock(&root, provider_url).state();
        let (addr, task) = spawn_server(state.clone()).await;
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("workspace dir");
        Self {
            base: format!("http://{addr}"),
            state,
            ws,
            task,
            _root: tmp,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// POSTs `body` to `/v1/sessions` and returns the raw response.
pub async fn post_create_session(base: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions"))
        .json(body)
        .send()
        .await
        .expect("create")
}

/// Creates a session over HTTP in `ws` and returns its id.
pub async fn create_session(base: &str, ws: &Path) -> String {
    let created: Value =
        post_create_session(base, &json!({ "workspace": ws.to_str().expect("utf8 ws") }))
            .await
            .json()
            .await
            .expect("create json");
    created["session_id"]
        .as_str()
        .expect("session id")
        .to_string()
}

/// The request body for a plain-text message to the main agent.
pub fn text_message(text: &str) -> Value {
    json!({
        "target_actor_id": "agent:main",
        "mode": "next-turn",
        "content": [{ "type": "text", "text": text }]
    })
}

/// POSTs an arbitrary message `body` to the session.
pub async fn post_message_body(base: &str, id: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/messages"))
        .json(body)
        .send()
        .await
        .expect("post message")
}

/// POSTs a plain-text message to the session.
pub async fn post_message(base: &str, id: &str, text: &str) -> reqwest::Response {
    post_message_body(base, id, &text_message(text)).await
}

/// [`post_message`], asserting the server accepted it.
pub async fn post_message_ok(base: &str, id: &str, text: &str) {
    let resp = post_message(base, id, text).await;
    assert_eq!(resp.status(), 200);
}

/// The command `POST /messages` builds for a plain-text message to the main
/// agent in session `id`.
pub fn text_command(id: ulid::Ulid, text: &str) -> Command {
    rust_agent::server::routes::build_command(
        id,
        &rust_agent::server::routes::PostMessageRequest {
            target_actor_id: "agent:main".to_string(),
            mode: Some("next-turn".to_string()),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        },
        DeliveryMode::NextTurn,
    )
}

/// Sends a plain-text message through the same core `POST /messages` uses.
pub async fn send_text(
    state: &AppState,
    id: ulid::Ulid,
    text: &str,
) -> rust_agent::session::MessageAccepted {
    state.send(text_command(id, text)).await.expect("routed")
}

/// One in-process request through the router. Returns the status and the
/// JSON body.
pub async fn request_json(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = app.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

/// Collects replay + live events until a LIVE event matches `pred`, then
/// settles briefly (same-tick sequenced events land in one batch) and drains
/// what is ready. Panics, listing the events seen, if nothing matches within
/// 20 s or the stream closes first.
pub async fn collect_until(
    sub: rust_agent::session::Subscription,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut out = Vec::new();
    if let rust_agent::bus::ResumeOutcome::Replay(replay) = sub.replay {
        out.extend(replay);
    }
    if let Some(mut live) = sub.live {
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let env = tokio::time::timeout(remaining, live.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for matching server event; seen: {out:?}")
                });
            let Some(env) = env else { break };
            let matched = pred(&env);
            out.push(env);
            if matched {
                tokio::time::sleep(Duration::from_millis(50)).await;
                while let Ok(env) = live.try_recv() {
                    out.push(env);
                }
                break;
            }
        }
    }
    assert!(
        out.iter().any(&pred),
        "stream closed before the matching server event; seen: {out:?}"
    );
    out
}

/// Polls `cond` until it holds, panicking with `what` after 20 s.
pub async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
