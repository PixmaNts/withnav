//! Process wiring for `nav lsp`: builds the model layers
//! from models.json and auth.json, starts the in-process cognition chat
//! server, assembles the [`NavigatorDeps`] and serves the Language Server
//! over stdio. A layer whose alias, key or config is missing is disabled
//! with a note and the others keep working.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::navigator::log::NavLog;
use crate::navigator::metacognition::LlmCommenter;
use crate::navigator::models::FimAdapter;
use crate::navigator::ports::FimModel;
use crate::navigator::{
    self, ConversationFeed, Metacognition, ModelError, NavigatorDeps, Remark, RemarkContext,
    ServerInfoParams,
};
use crate::provider::auth;
use crate::provider::catalog::{resolve_chat, ModelCatalog};
use crate::provider::config::ModelsConfig;
use crate::provider::fim::InceptionFim;
use crate::provider::openai::ClientCache;

/// Resolves the models config: explicit path, workspace copy (it carries the
/// navigator aliases), then the config dir.
pub fn load_models(explicit: Option<&Path>) -> anyhow::Result<ModelsConfig> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    crate::provider::config::load_models(explicit, &workspace, &config_dir)
        .map(|(config, _path)| config)
        .map_err(|e| anyhow::anyhow!("cannot load models.json: {e}"))
}

struct Layers {
    fim: Option<Arc<dyn FimModel>>,
    reflex_model: Option<String>,
    instinct_model: Option<String>,
    metacognition_model: Option<String>,
    metacognition: Option<Arc<LazyCommenter>>,
    disabled: Vec<String>,
}

/// Builds optional layers from the admitted workspace catalog and explicit directories.
fn build_layers_from(
    models: &ModelsConfig,
    config_dir: &Path,
    catalog: &ModelCatalog,
    metacognition_log: PathBuf,
    clients: Arc<ClientCache>,
) -> Layers {
    let mut layers = Layers {
        fim: None,
        reflex_model: None,
        instinct_model: None,
        metacognition_model: None,
        metacognition: None,
        disabled: Vec::new(),
    };
    // Reflex uses the configured FIM assignment.
    match crate::provider::layers::assigned(
        models,
        config_dir,
        crate::provider::assignments::Layer::Reflex,
    ) {
        Ok(assigned) => {
            let resolved = assigned.resolved;
            let model_id = resolved.model.model.clone();
            match auth::resolve_key(
                config_dir,
                &resolved.provider_name,
                resolved.provider.api_key_env.as_deref(),
            ) {
                Ok(key) => {
                    layers.fim = Some(Arc::new(FimAdapter(InceptionFim::new(
                        resolved,
                        key,
                        clients.clone(),
                    ))));
                    layers.reflex_model = Some(model_id);
                }
                Err(e) => {
                    tracing::error!("reflex disabled (no API key): {e}");
                    layers.disabled.push(format!("reflex: no API key ({e})"));
                }
            }
        }
        Err(e) => {
            tracing::error!("reflex disabled (alias): {e}");
            layers.disabled.push(format!("reflex: {e}"));
        }
    }

    // Check the instinct assignment used by the native flow host.
    match crate::provider::layers::instinct(models, config_dir, clients.clone()) {
        Ok(client) => {
            layers.instinct_model = Some(client.model().to_string());
        }
        Err(e) => {
            tracing::error!("instinct disabled: {e}");
            layers.disabled.push(format!("instinct: {e}"));
        }
    }

    // Metacognition: the configured fast chat commenter,
    // logging to the navigator log. Only its config and key are checked
    // here; the context window is resolved on first use, so startup never
    // waits for the provider's model list.
    match LazyCommenter::new(
        models,
        "metacognition",
        config_dir,
        catalog,
        clients.clone(),
        metacognition_log,
    ) {
        Ok(commenter) => {
            layers.metacognition_model = Some(commenter.model_id().to_string());
            layers.metacognition = Some(Arc::new(commenter));
        }
        Err(e) => {
            // Only the remarks go: FIM, instinct, history and the server do
            // not depend on this alias.
            tracing::warn!("metacognition remarks disabled: {e}");
            layers.disabled.push(format!("metacognition: {e}"));
        }
    }

    layers
}

/// The metacognition commenter whose context window is resolved on first
/// use, by the same resolver as a chat session: an alias with no known
/// window is refused then, with one warning, and only the remarks stop. The
/// bounds, once obtained, are kept for the life of the process; a failure is
/// retried on a later use (the catalog backs off between attempts).
pub(crate) struct LazyCommenter {
    assigned: crate::provider::assignments::AssignedModel,
    resolved: crate::provider::config::ResolvedModel,
    api_key: String,
    catalog: ModelCatalog,
    clients: Arc<ClientCache>,
    log_path: PathBuf,
    commenter: tokio::sync::OnceCell<Arc<LlmCommenter>>,
    warned: std::sync::atomic::AtomicBool,
}

impl LazyCommenter {
    pub(crate) fn new(
        models: &ModelsConfig,
        _alias: &str,
        config_dir: &Path,
        catalog: &ModelCatalog,
        clients: Arc<ClientCache>,
        log_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let assigned = crate::provider::layers::assigned(
            models,
            config_dir,
            crate::provider::assignments::Layer::Metacognition,
        )?;
        let resolved = assigned.resolved.clone();
        let api_key = auth::resolve_key(
            config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )
        .map_err(|e| {
            anyhow::anyhow!("no API key for provider '{}': {e}", resolved.provider_name)
        })?;
        Ok(Self {
            assigned,
            resolved,
            api_key,
            catalog: catalog.clone(),
            clients,
            log_path,
            commenter: tokio::sync::OnceCell::new(),
            warned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The model identifier sent on the wire.
    fn model_id(&self) -> &str {
        &self.resolved.model.model
    }

    /// The commenter, resolving its window the first time.
    async fn get(&self) -> Result<&Arc<LlmCommenter>, ModelError> {
        self.commenter
            .get_or_try_init(|| async {
                let chat = resolve_chat(&self.catalog, &self.resolved, &self.api_key, &self.clients)
                    .await
                    .map_err(|e| {
                        format!(
                            "alias '{}' (provider '{}', model '{}') has no known context window; set \"context_window\" for it in models.json",
                            e.alias, e.provider, e.model
                        )
                    })?;
                let variant = crate::provider::layers::chat_variant(&self.assigned, &chat)
                    .map_err(|e| e.to_string())?;
                let mut resolved = self.resolved.clone();
                resolved.model.context_window = Some(chat.window);
                resolved.model.max_input_tokens = chat.max_input_tokens;
                Ok::<_, String>(Arc::new(
                    LlmCommenter::new(resolved, self.api_key.clone(), self.clients.clone())
                        .with_variant(variant)
                        .with_log(NavLog::new(self.log_path.clone())),
                ))
            })
            .await
            .map_err(|reason| {
                if !self
                    .warned
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!("metacognition remarks disabled: {reason}");
                }
                ModelError::Unavailable(format!("metacognition: {reason}"))
            })
    }
}

impl Metacognition for LazyCommenter {
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
        Box::pin(async move { self.get().await?.remark(ctx, cancel).await })
    }
}

impl crate::navigator::ports::ReflexContextModel for LazyCommenter {
    fn refresh<'a>(
        &'a self,
        request: &'a crate::navigator::ports::ReflexContextRequest,
    ) -> futures::future::BoxFuture<'a, Result<String, ModelError>> {
        Box::pin(async move { self.get().await?.refresh(request).await })
    }
}

impl Layers {
    /// The metacognition seam: the real commenter when built, else a
    /// failing stand-in so the editor never crashes.
    fn metacognition(&self) -> Arc<dyn Metacognition> {
        match &self.metacognition {
            Some(llm) => Arc::clone(llm) as Arc<dyn Metacognition>,
            None => Arc::new(FailingCommenter {
                message: "metacognition layer disabled".to_string(),
            }),
        }
    }
}

/// Builds the layers, starts the cognition chat server and serves the
/// navigator over stdio until the client exits.
pub async fn run(models_path: Option<&Path>) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt as _;
    let cwd = std::env::current_dir()?;
    let mut stdin = tokio::io::stdin();
    let Some((frame, initialized_workspace)) = initial_workspace(&mut stdin, &cwd).await? else {
        return Ok(());
    };
    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let preflight = crate::session::preflight::Preflight::capture(
        &config_dir,
        models_path,
        &initialized_workspace,
        &crate::agent::instructions::RulesRoots::from_user_home(),
    )
    .await?;
    preflight.check(&initialized_workspace).await?;
    let clients = Arc::new(ClientCache::new());

    let log_path = crate::paths::decision_log()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve user data dir"))?;
    let mut layers = build_layers_from(
        preflight.models(),
        &config_dir,
        &ModelCatalog::system(),
        log_path.clone(),
        clients,
    );
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Admission, layers and cognition all use the initialize workspace catalog.
    let workspace = Some(initialized_workspace);
    let user_dir = navigator::default_user_dir();
    let root = workspace.clone().unwrap_or_else(|| PathBuf::from("."));

    // The cognition chat rides on an in-process Phase-2 server bound to a
    // loopback ephemeral port with a per-process bearer token, announced as
    // `withnav/serverInfo`. It lives exactly as long as the LSP session:
    // when `navigator::serve` returns (exit/EOF) the shutdown trigger fires,
    // every session is closed and the server drains so the process exits.
    // The SAME conversation hub goes to the session server (sessions
    // register themselves there) and to the navigator deps (wakes post
    // through it): one process, two channels, one conversation.
    let conversation = ConversationFeed::new();
    let http = match spawn_cognition_server(preflight.models_path(), conversation.clone()).await {
        Ok(http) => Some(http),
        Err(e) => {
            // A failed bind or an unopenable history database must not take
            // the navigator down: run the LSP without the chat (no
            // serverInfo notification) and say so in `withnav/status`.
            tracing::error!("cognition chat server unavailable: {e}");
            layers.disabled.push(format!("chat: {e}"));
            None
        }
    };
    let flow_host = http
        .as_ref()
        .map(|http| http.flows.clone())
        .unwrap_or_else(|| {
            Arc::new(crate::flows::host::Host::new(
                config_dir.clone(),
                crate::paths::data_dir().unwrap_or_else(|| config_dir.join("data")),
                Some(preflight.models_path().to_path_buf()),
            ))
        });
    let flow_factory = flow_host.runtime(&root).map(|runtime| {
        Arc::new(move |effects| runtime.with_effects_adapter(effects))
            as Arc<crate::navigator::FlowFactory>
    });

    let mut deps = NavigatorDeps::new(layers.metacognition(), log_path)
        .with_dirs(workspace, user_dir)
        .with_conversation(conversation)
        .with_model_ids(
            layers.reflex_model.clone(),
            layers.instinct_model.clone(),
            layers.metacognition_model.clone(),
        )
        .with_disabled_layers(layers.disabled.clone());
    deps.flows = flow_factory;
    deps.startup_warnings = flow_host
        .store(root.clone())
        .migration_diagnostics()
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect();
    if let Some(info) = http.as_ref().map(|h| h.info.clone()) {
        deps = deps.with_server_info(info);
    }
    if let Some(fim) = layers.fim {
        deps = deps.with_fim(fim);
    }
    if let Some(llm) = layers.metacognition {
        deps = deps.with_reflex_context(llm);
    }

    navigator::serve(
        std::io::Cursor::new(frame).chain(stdin),
        tokio::io::stdout(),
        deps,
    )
    .await;

    // LSP exit/EOF: close every chat session and let the server drain, so
    // the process exits instead of leaking the HTTP task.
    if let Some(http) = http {
        let _ = http.shutdown.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), http.task).await;
    }
    let _ = tokio::task::spawn_blocking(move || flow_host.shutdown()).await;
    Ok(())
}

/// Reads and replays the first initialize frame: an editor's cwd is not its project.
/// Bounded framing keeps untrusted stdio input from allocating without a limit.
async fn initial_workspace<R: tokio::io::AsyncRead + Unpin>(
    input: &mut R,
    cwd: &Path,
) -> anyhow::Result<Option<(Vec<u8>, PathBuf)>> {
    use tokio::io::AsyncReadExt as _;
    let mut frame = Vec::new();
    loop {
        let mut byte = [0];
        if input.read(&mut byte).await? == 0 {
            anyhow::ensure!(frame.is_empty(), "incomplete LSP initialize header");
            return Ok(None);
        }
        frame.push(byte[0]);
        anyhow::ensure!(
            frame.len() <= 8192,
            "LSP initialize header exceeds 8192 bytes"
        );
        if frame.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&frame)?;
    let lengths: Vec<_> = header
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(
        lengths.len() == 1,
        "LSP initialize requires one Content-Length"
    );
    let length = lengths[0];
    anyhow::ensure!(length <= 1024 * 1024, "LSP initialize body exceeds 1 MiB");
    let start = frame.len();
    frame.resize(start + length, 0);
    input.read_exact(&mut frame[start..]).await?;
    let message: serde_json::Value = serde_json::from_slice(&frame[start..])?;
    anyhow::ensure!(
        message["method"] == "initialize",
        "first LSP request must be initialize"
    );
    let params: tower_lsp_server::ls_types::InitializeParams =
        serde_json::from_value(message["params"].clone()).map_err(|error| {
            anyhow::anyhow!("invalid LSP initialize workspace or parameters: {error}")
        })?;
    #[allow(deprecated)]
    let supplied_workspace = params.root_uri.is_some()
        || params
            .workspace_folders
            .as_ref()
            .is_some_and(|folders| !folders.is_empty());
    #[allow(deprecated)]
    let root = params
        .root_uri
        .as_ref()
        .and_then(super::lsp::uri::uri_to_path);
    let folder = params.workspace_folders.as_ref().and_then(|folders| {
        folders
            .iter()
            .find_map(|f| super::lsp::uri::uri_to_path(&f.uri))
    });
    let workspace = match root.or(folder) {
        Some(workspace) => workspace,
        None if !supplied_workspace => cwd.to_path_buf(),
        None => anyhow::bail!(
            "LSP initialize supplied a workspace but no supported local file path; provide a valid file:// rootUri or workspace folder"
        ),
    };
    Ok(Some((frame, workspace)))
}

/// The running cognition chat server: the spawned HTTP task plus the
/// shutdown trigger the LSP exit fires, and the endpoint to announce.
struct CognitionHttp {
    task: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    info: ServerInfoParams,
    flows: Arc<crate::flows::host::Host>,
}

/// The in-process cognition server: an [`AppState`] over the same models
/// resolution as the LSP layers, bound to a loopback ephemeral port, with a
/// random 64-hex-char bearer token.
///
/// [`AppState`]: crate::server::session::AppState
async fn spawn_cognition_server(
    models_path: &Path,
    conversation: ConversationFeed,
) -> anyhow::Result<CognitionHttp> {
    use crate::server::session::{AppState, ServerConfig};

    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let token = bearer_token();
    // Conversation history: one SQLite file in WithNav's data dir.
    let history_db = crate::paths::history_db()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))?;
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path: Some(models_path.to_path_buf()),
        config_dir,
        allow_non_loopback: false,
        bearer_token: Some(token.clone()),
        history_db,
        data_dir: crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
        rules_roots: crate::agent::instructions::RulesRoots::from_user_home(),
        web: None,
    };
    let state = AppState::with_hooks(
        config,
        Arc::new(navigator::NavigatorHooks::with_feed(conversation)),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let flows = state.service().flows.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = crate::server::routes::serve_until(state, listener, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    Ok(CognitionHttp {
        task,
        shutdown: shutdown_tx,
        info: ServerInfoParams { port, token },
        flows,
    })
}

/// A random 32-byte bearer token, hex-encoded: two ULIDs give 128 random
/// bits each with millisecond ordering, plenty for a per-process secret.
fn bearer_token() -> String {
    use ulid::Ulid;
    format!("{}{}", Ulid::generate(), Ulid::generate()).to_lowercase()
}

/// Stand-in used when the metacognition layer could not be built: every
/// remark call errors, which surfaces as `withnav/status` error instead
/// of a crash.
struct FailingCommenter {
    message: String,
}

impl Metacognition for FailingCommenter {
    fn remark<'a>(
        &'a self,
        _ctx: &'a RemarkContext,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
        let msg = self.message.clone();
        Box::pin(async move { Err(ModelError::Unavailable(msg)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider nothing listens on, with the three navigator aliases on
    /// it. `metacognition_window` of `None` leaves that alias without a
    /// window, so only discovery could supply one.
    fn config(metacognition_window: Option<u64>) -> ModelsConfig {
        let mut metacognition = serde_json::json!({
            "provider": "omni", "model": "m-chat", "supports_tools": true
        });
        if let Some(w) = metacognition_window {
            metacognition["context_window"] = w.into();
        }
        ModelsConfig::parse(
            &serde_json::json!({
                "providers": { "omni": {
                    "base_url": "http://127.0.0.1:1/v1",
                    "connect_timeout_ms": 200, "request_timeout_ms": 500
                }},
                "models": {
                    "omni/m-chat": metacognition,
                    "omni/m-fim": { "provider": "omni", "model": "m-fim", "api": "fim" },
                    "omni/m-s1": { "provider": "omni", "model": "m-s1", "api": "systemone" }
                }
            })
            .to_string(),
        )
        .expect("config")
    }

    fn layers(models: &ModelsConfig) -> (tempfile::TempDir, Layers) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("cfg");
        auth::set_key(&cfg, "omni", "test-key").expect("key");
        std::fs::write(cfg.join("config.json"), r#"{"layers":{"reflex":"omni/m-fim","instinct":"omni/m-s1","metacognition":"omni/m-chat"}}"#).unwrap();
        let layers = build_layers_from(
            models,
            &cfg,
            &ModelCatalog::new(Some(dir.path().join("data"))),
            dir.path().join("log.jsonl"),
            Arc::new(ClientCache::new()),
        );
        (dir, layers)
    }

    fn ctx() -> RemarkContext {
        RemarkContext::build("file:///a.rs", "rust", "a", "b", Vec::new(), None)
    }

    // Given a metacognition alias with no window and a provider that cannot
    //       be reached
    // When the layers are built, then used
    // Then building touches nothing; the first remark fails with the reason
    //      and only the remarks are affected: FIM and the instinct stay up
    #[tokio::test]
    async fn an_unresolved_metacognition_disables_only_the_remarks() {
        let (_dir, layers) = layers(&config(None));
        assert!(layers.fim.is_some(), "FIM is not touched");
        assert!(
            layers.instinct_model.is_some(),
            "the instinct is not touched"
        );
        assert!(layers.disabled.is_empty(), "{:?}", layers.disabled);
        let cancel = tokio_util::sync::CancellationToken::new();
        for _ in 0..2 {
            let err = layers
                .metacognition()
                .remark(&ctx(), &cancel)
                .await
                .expect_err("no window from any source");
            let text = err.to_string();
            assert!(
                text.contains("no known context window") && text.contains("'omni/m-chat'"),
                "{text}"
            );
        }
    }

    // Given the same alias with its own window
    // When the layers are built
    // Then the commenter is there and nothing is disabled
    #[tokio::test]
    async fn a_metacognition_with_a_window_is_built() {
        let (_dir, layers) = layers(&config(Some(128_000)));
        assert!(layers.metacognition.is_some());
        assert_eq!(layers.metacognition_model.as_deref(), Some("m-chat"));
        assert!(layers.fim.is_some() && layers.instinct_model.is_some());
        assert!(layers.disabled.is_empty(), "{:?}", layers.disabled);
    }

    // Given a provider whose model list is slower than the deadline
    // When the layers are built
    // Then building returns at once without a request; the list is asked for
    //      on the first remark, and a window it reported is kept
    #[tokio::test]
    async fn the_window_is_resolved_on_first_use_and_kept() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": [{
                        "id": "m-chat", "context_length": 64_000
                    }]}))
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        let models = ModelsConfig::parse(
            &serde_json::json!({
                "providers": { "omni": { "base_url": format!("{}/v1", server.uri()) }},
                "models": { "omni/m-chat": {
                    "provider": "omni", "model": "m-chat", "supports_tools": true
                }}
            })
            .to_string(),
        )
        .expect("config");
        let started = std::time::Instant::now();
        let (_dir, layers) = layers(&models);
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        assert!(server.received_requests().await.unwrap().is_empty());

        let lazy = layers.metacognition.expect("built");
        assert!(lazy.get().await.is_ok());
        assert!(lazy.get().await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
