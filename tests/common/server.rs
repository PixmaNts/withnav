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

use navai::agent::instructions::RulesRoots;
use navai::bus::command::{Command, ContentBlock, DeliveryMode};
use navai::bus::event::EventEnvelope;
use navai::server::{AppState, ServerConfig};

use super::sse::spawn_mock_provider;

/// An explicit driver message, rather than a parent-authored child prompt.
pub fn driver_command(session: ulid::Ulid, text: String) -> Command {
    Command {
        command_id: ulid::Ulid::new(),
        session_id: session,
        source_actor_id: "user:test".into(),
        target_actor_id: "agent:main".into(),
        mode: DeliveryMode::NextTurn,
        content: vec![ContentBlock::Text { text }],
        created_at: std::time::SystemTime::now(),
    }
}

/// A provider URL nothing listens on. Creating a session loads the models
/// file but makes no network call, so it is never dialed.
pub const UNREACHABLE_PROVIDER_URL: &str = "http://127.0.0.1:1/v1";

/// Name of the provider in the generated `models.json` and `auth.json`.
pub const PROVIDER: &str = "mock";

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
    rules_roots: RulesRoots,
    chat_tools: Option<u64>,
    without_window: bool,
    extra_aliases: Vec<(String, String)>,
    variants: Option<Value>,
    default_variant: Option<String>,
    omniroute: bool,
    web: Option<navai::server::WebConfig>,
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
            rules_roots: RulesRoots::default(),
            chat_tools: None,
            without_window: false,
            extra_aliases: Vec::new(),
            variants: None,
            default_variant: None,
            omniroute: false,
            web: None,
        }
    }

    /// Marks the alias as a chat model with tools and a context window of
    /// `context_window` tokens, so `GET /v1/models` lists it.
    pub fn chat_tools(mut self, context_window: u64) -> Self {
        self.chat_tools = Some(context_window);
        self
    }

    /// Leaves the alias without a `context_window`, as a models.json written
    /// before windows were required.
    pub fn without_window(mut self) -> Self {
        self.without_window = true;
        self
    }

    /// Path of the `models.json` [`Self::config`] writes.
    pub fn models_path(&self) -> PathBuf {
        self.root.join("models.json")
    }

    /// Names the single model alias the `models.json` defines.
    pub fn model_alias(mut self, alias: &str, model: &str) -> Self {
        self.alias = alias.to_string();
        self.model = model.to_string();
        self
    }

    /// Adds a second alias on the same provider, with the same chat/tools
    /// settings as the first.
    pub fn extra_alias(mut self, alias: &str, model: &str) -> Self {
        self.extra_aliases
            .push((alias.to_string(), model.to_string()));
        self
    }

    /// Gives the alias these `variants` (a JSON object by name) and,
    /// optionally, a `default_variant`.
    pub fn variants(mut self, variants: Value, default: Option<&str>) -> Self {
        self.variants = Some(variants);
        self.default_variant = default.map(str::to_string);
        self
    }

    /// Marks the provider as an OmniRoute one: an alias without `variants`
    /// then offers one variant per effort tier its model list reports.
    pub fn omniroute(mut self) -> Self {
        self.omniroute = true;
        self
    }

    /// A `default_variant` on an alias with no `variants` of its own.
    pub fn default_variant(mut self, name: &str) -> Self {
        self.default_variant = Some(name.to_string());
        self
    }

    /// Serves the browser client's files too, as `navai web` does, with these launch workspaces.
    pub fn web(mut self, launch_workspaces: Vec<PathBuf>) -> Self {
        self.web = Some(navai::server::WebConfig { launch_workspaces });
        self
    }

    pub fn bearer_token(mut self, token: &str) -> Self {
        self.bearer_token = Some(token.to_string());
        self
    }

    /// Reads the global rules from `home` instead of none.
    pub fn rules_home(mut self, home: &Path) -> Self {
        self.rules_roots = RulesRoots {
            home: Some(home.to_path_buf()),
        };
        self
    }

    pub fn history_db(mut self, path: PathBuf) -> Self {
        self.history_db = Some(path);
        self
    }

    /// Writes the models and auth files and returns the config.
    pub fn config(&self) -> ServerConfig {
        let models_path = self.root.join("models.json");
        let mut alias_entry = json!({
            "provider": PROVIDER,
            "model": self.model,
            "reasoning_history": "preserve",
            "context_window": 128000
        });
        if let Some(window) = self.chat_tools {
            alias_entry["supports_tools"] = json!(true);
            alias_entry["context_window"] = json!(window);
        }
        if self.without_window {
            alias_entry
                .as_object_mut()
                .unwrap()
                .remove("context_window");
        }
        if let Some(variants) = &self.variants {
            alias_entry["variants"] = variants.clone();
        }
        if let Some(default) = &self.default_variant {
            alias_entry["default_variant"] = json!(default);
        }
        let mut models = serde_json::Map::new();
        let mut layers = serde_json::Map::new();
        let primary = format!("{PROVIDER}/{}", self.model);
        models.insert(primary.clone(), alias_entry.clone());
        for (alias, model) in &self.extra_aliases {
            let mut entry = alias_entry.clone();
            entry["model"] = json!(model);
            if matches!(alias.as_str(), "instinct" | "metacognition" | "reflex") {
                entry["api"] = json!(match alias.as_str() {
                    "instinct" => "systemone",
                    "reflex" => "fim",
                    _ => "chat",
                });
                let name = format!("{PROVIDER}/{model}");
                models.insert(name.clone(), entry.clone());
                layers.insert(alias.clone(), json!(name));
                continue;
            }
            models.insert(format!("{PROVIDER}/{model}"), entry);
        }
        let mut provider = json!({
            "base_url": self.base_url,
            "api_key_env": "NAVAI_TEST_KEY",
            "headers": {},
            "connect_timeout_ms": self.connect_timeout_ms,
            "request_timeout_ms": self.request_timeout_ms
        });
        if self.omniroute {
            provider["catalog_format"] = json!("omniroute");
        }
        std::fs::write(
            &models_path,
            json!({
                "providers": { PROVIDER: provider },
                "models": models
            })
            .to_string(),
        )
        .expect("write models.json");
        let config_dir = self.root.join("cfg");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("config.json"),
            json!({"permissions":{"scratch_dirs":[]}, "layers":layers, "agents":{"build":{"model":primary},"plan":{"model":primary}}}).to_string(),
        )
        .expect("isolated scratch settings");
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
            data_dir: self.root.join("data"),
            rules_roots: self.rules_roots.clone(),
            web: self.web.clone(),
        }
    }

    /// An in-process [`AppState`] over [`Self::config`].
    pub fn state(&self) -> AppState {
        AppState::new(self.config()).expect("state over a temp history database")
    }

    /// Like [`Self::state`], with editor hooks.
    pub fn navigator_state(&self) -> AppState {
        self.navigator_state_with_conversation(None)
    }

    /// [`Self::navigator_state`] sharing the navigator's conversation feed.
    pub fn navigator_state_with_conversation(
        &self,
        conversation: Option<navai::navigator::ConversationFeed>,
    ) -> AppState {
        self.editor_state(conversation, false)
    }

    /// Editor hooks with an explicitly configured build model, not a hook default.
    pub fn assigned_navigator_state(&self) -> AppState {
        self.assigned_navigator_state_with_conversation(None)
    }

    pub fn assigned_navigator_state_with_conversation(
        &self,
        conversation: Option<navai::navigator::ConversationFeed>,
    ) -> AppState {
        self.editor_state(conversation, true)
    }

    fn editor_state(
        &self,
        conversation: Option<navai::navigator::ConversationFeed>,
        assigned_build: bool,
    ) -> AppState {
        let hooks = match conversation {
            Some(feed) => navai::navigator::NavigatorHooks::with_feed(feed),
            None => navai::navigator::NavigatorHooks::new(),
        };
        let assigned = format!("{PROVIDER}/{}", self.model);
        let config = self.config();
        let path = config.config_dir.join("config.json");
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("isolated settings"))
                .expect("settings JSON");
        if assigned_build {
            settings["agents"] = json!({"build": {"model": assigned}});
        }
        std::fs::write(path, settings.to_string()).expect("explicit build assignment");
        AppState::with_hooks(config, std::sync::Arc::new(hooks))
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
        let _ = navai::server::routes::serve_state_on(state, listener).await;
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
        Self::start_inner(provider_url, None).await
    }

    /// Like [`Self::start`], but every route except `/v1/health` demands
    /// `Authorization: Bearer <token>`.
    pub async fn start_with_token(mock_responses: Vec<Vec<u8>>, token: &str) -> Self {
        let mock = spawn_mock_provider(mock_responses).await;
        Self::start_inner(&mock.url, Some(token)).await
    }

    async fn start_inner(provider_url: &str, token: Option<&str>) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let mut config = TestServerConfig::mock(&root, provider_url);
        if let Some(token) = token {
            config = config.bearer_token(token);
        }
        let state = config.state();
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
    navai::server::routes::build_command(
        id,
        &navai::server::routes::PostMessageRequest {
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
) -> navai::session::MessageAccepted {
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
    sub: navai::session::Subscription,
    pred: impl Fn(&EventEnvelope) -> bool,
) -> Vec<EventEnvelope> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut out = Vec::new();
    if let navai::bus::ResumeOutcome::Replay(replay) = sub.replay {
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

/// A TCP forwarder in front of a server that can drop every open connection
/// on demand, to provoke a lost event stream. New connections still work.
pub struct CuttableProxy {
    pub base: String,
    connections: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl CuttableProxy {
    /// Forwards `127.0.0.1:<ephemeral>` to the server at `target` (a base URL
    /// such as `http://127.0.0.1:4000`).
    pub async fn start(target: &str) -> Self {
        let target = target.trim_start_matches("http://").to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy bind");
        let addr = listener.local_addr().expect("proxy addr");
        let connections: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Default::default();
        let tracked = connections.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let target = target.clone();
                let task = tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                tracked.lock().expect("proxy connections").push(task);
            }
        });
        Self {
            base: format!("http://{addr}"),
            connections,
            accept,
        }
    }

    /// Drops every connection that is open right now.
    pub fn cut(&self) {
        for task in self
            .connections
            .lock()
            .expect("proxy connections")
            .drain(..)
        {
            task.abort();
        }
    }
}

impl Drop for CuttableProxy {
    fn drop(&mut self) {
        self.cut();
        self.accept.abort();
    }
}

/// Points the model resolution saved on session `id` at `endpoint`, so a
/// resume against another scripted provider is not refused as a moved
/// endpoint.
pub fn move_saved_endpoint(db: &Path, id: &str, endpoint: &str) {
    let conn = rusqlite::Connection::open(db).expect("open history db");
    let raw: String = conn
        .query_row(
            "SELECT model_resolution FROM sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .expect("saved resolution");
    let mut saved: serde_json::Value = serde_json::from_str(&raw).expect("resolution json");
    saved["endpoint"] = json!(endpoint);
    conn.execute(
        "UPDATE sessions SET model_resolution = ?2 WHERE id = ?1",
        rusqlite::params![id, saved.to_string()],
    )
    .expect("move the saved endpoint");
}
