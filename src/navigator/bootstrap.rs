//! Process wiring for the `navai-lsp` binary: builds the model layers
//! from models.json and auth.json, starts the in-process cognition chat
//! server, assembles the [`NavigatorDeps`] and serves the Language Server
//! over stdio. A layer whose alias, key or config is missing is disabled
//! with a note and the others keep working.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::navigator::log::NavLog;
use crate::navigator::metacognition::LlmCommenter;
use crate::navigator::models::{DecisionAdapter, FimAdapter};
use crate::navigator::ports::{DecisionModel, FimModel};
use crate::navigator::{
    self, ConversationFeed, Metacognition, ModelError, NavigatorDeps, Remark, RemarkContext,
    ServerInfoParams,
};
use crate::provider::auth;
use crate::provider::catalog::{resolve_chat, ModelCatalog};
use crate::provider::config::ModelsConfig;
use crate::provider::fim::InceptionFim;
use crate::provider::openai::ClientCache;
use crate::provider::systemone::SystemOneClient;

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
    decision: Option<Arc<dyn DecisionModel>>,
    reflex_model: Option<String>,
    instinct_model: Option<String>,
    metacognition_model: Option<String>,
    metacognition: Option<Arc<LlmCommenter>>,
    disabled: Vec<String>,
}

/// Builds every layer from its alias. A missing alias or key disables that
/// layer with a note; the others keep working.
async fn build_layers(models_path: Option<&Path>, clients: Arc<ClientCache>) -> Layers {
    let models = match load_models(models_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("models.json unavailable, every model layer disabled: {e}");
            return Layers::all_disabled(format!("all: models.json ({e})"));
        }
    };
    let config_dir = auth::default_config_dir().unwrap_or_else(|| PathBuf::from("."));
    let log_path = crate::paths::decision_log().unwrap_or_else(|| PathBuf::from("/dev/null"));
    build_layers_from(
        &models,
        &config_dir,
        &ModelCatalog::system(),
        log_path,
        clients,
    )
    .await
}

impl Layers {
    fn all_disabled(note: String) -> Self {
        Self {
            fim: None,
            decision: None,
            reflex_model: None,
            instinct_model: None,
            metacognition_model: None,
            metacognition: None,
            disabled: vec![note],
        }
    }
}

/// [`build_layers`] over an already loaded config and explicit directories.
async fn build_layers_from(
    models: &ModelsConfig,
    config_dir: &Path,
    catalog: &ModelCatalog,
    metacognition_log: PathBuf,
    clients: Arc<ClientCache>,
) -> Layers {
    let mut layers = Layers {
        fim: None,
        decision: None,
        reflex_model: None,
        instinct_model: None,
        metacognition_model: None,
        metacognition: None,
        disabled: Vec::new(),
    };
    // Reflex: Mercury FIM on the `reflex` alias.
    match models.resolve("reflex") {
        Ok(resolved) => {
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

    // Instinct: System One on the `instinct` alias (the pipeline snapshot
    // names its own alias; the shipped ones all use `instinct`).
    match SystemOneClient::from_alias(models, "instinct", config_dir, clients.clone()) {
        Ok(client) => {
            layers.instinct_model = Some(client.model().to_string());
            layers.decision = Some(Arc::new(DecisionAdapter(client)));
        }
        Err(e) => {
            tracing::error!("instinct disabled: {e}");
            layers.disabled.push(format!("instinct: {e}"));
        }
    }

    // Metacognition: the fast chat commenter on the `metacognition` alias,
    // logging to the navigator log.
    match build_llm_commenter(
        models,
        "metacognition",
        config_dir,
        catalog,
        clients.clone(),
        metacognition_log,
    )
    .await
    {
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

/// Chat commenter on a named alias (the metacognition).
async fn build_llm_commenter(
    models: &ModelsConfig,
    alias: &str,
    config_dir: &Path,
    catalog: &ModelCatalog,
    clients: Arc<ClientCache>,
    log_path: PathBuf,
) -> anyhow::Result<LlmCommenter> {
    let resolved = models
        .resolve(alias)
        .map_err(|e| anyhow::anyhow!("cannot resolve alias '{alias}': {e}"))?;
    let api_key = auth::resolve_key(
        config_dir,
        &resolved.provider_name,
        resolved.provider.api_key_env.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("no API key for provider '{}': {e}", resolved.provider_name))?;
    // The same resolver as a chat session, before the first call: an alias
    // with no known window is refused here, not discovered at remark time.
    resolve_chat(catalog, &resolved, &api_key, &clients)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "alias '{}' (provider '{}', model '{}') has no known context window; set \"context_window\" for it in models.json",
                e.alias,
                e.provider,
                e.model
            )
        })?;
    Ok(LlmCommenter::new(resolved, api_key, clients).with_log(NavLog::new(log_path)))
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
    let clients = Arc::new(ClientCache::new());
    let mut layers = build_layers(models_path, clients).await;

    let log_path = crate::paths::decision_log()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve user data dir"))?;
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // The workspace root starts at the process cwd; initialize upgrades it
    // from rootUri when the client sends one.
    let workspace = std::env::current_dir().ok();
    let user_dir = navigator::default_user_dir();

    // The cognition chat rides on an in-process Phase-2 server bound to a
    // loopback ephemeral port with a per-process bearer token, announced as
    // `navigator/serverInfo`. It lives exactly as long as the LSP session:
    // when `navigator::serve` returns (exit/EOF) the shutdown trigger fires,
    // every session is closed and the server drains so the process exits.
    // The SAME conversation hub goes to the session server (sessions
    // register themselves there) and to the navigator deps (wakes post
    // through it): one process, two channels, one conversation.
    let conversation = ConversationFeed::new();
    let http = match spawn_cognition_server(models_path, conversation.clone()).await {
        Ok(http) => Some(http),
        Err(e) => {
            // A failed bind or an unopenable history database must not take
            // the navigator down: run the LSP without the chat (no
            // serverInfo notification) and say so in `navigator/status`.
            tracing::error!("cognition chat server unavailable: {e}");
            layers.disabled.push(format!("chat: {e}"));
            None
        }
    };

    let mut deps = NavigatorDeps::new(layers.metacognition(), log_path)
        .with_dirs(workspace, user_dir)
        .with_conversation(conversation)
        .with_model_ids(
            layers.reflex_model.clone(),
            layers.instinct_model.clone(),
            layers.metacognition_model.clone(),
        )
        .with_disabled_layers(layers.disabled.clone());
    if let Some(info) = http.as_ref().map(|h| h.info.clone()) {
        deps = deps.with_server_info(info);
    }
    if let Some(fim) = layers.fim {
        deps = deps.with_fim(fim);
    }
    if let Some(decision) = layers.decision {
        deps = deps.with_decision(decision);
    }
    if let Some(llm) = layers.metacognition {
        deps = deps.with_reflex_context(llm);
    }

    navigator::serve(tokio::io::stdin(), tokio::io::stdout(), deps).await;

    // LSP exit/EOF: close every chat session and let the server drain, so
    // the process exits instead of leaking the HTTP task.
    if let Some(http) = http {
        let _ = http.shutdown.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), http.task).await;
    }
    Ok(())
}

/// The running cognition chat server: the spawned HTTP task plus the
/// shutdown trigger the LSP exit fires, and the endpoint to announce.
struct CognitionHttp {
    task: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    info: ServerInfoParams,
}

/// The in-process cognition server: an [`AppState`] over the same models
/// resolution as the LSP layers, bound to a loopback ephemeral port, with a
/// random 64-hex-char bearer token.
///
/// [`AppState`]: crate::server::session::AppState
async fn spawn_cognition_server(
    models_path: Option<&Path>,
    conversation: ConversationFeed,
) -> anyhow::Result<CognitionHttp> {
    use crate::server::session::{AppState, ServerConfig};

    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let models_path = navigator::resolve_models(models_path, &workspace);
    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let token = bearer_token();
    // Conversation history: one SQLite file in Navai's data dir.
    let history_db = crate::paths::history_db()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the Navai data dir"))?;
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path,
        config_dir,
        allow_non_loopback: false,
        bearer_token: Some(token.clone()),
        history_db,
        data_dir: crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
        rules_roots: crate::agent::instructions::RulesRoots::from_user_home(),
    };
    let state = AppState::with_hooks(
        config,
        Arc::new(navigator::NavigatorHooks::with_feed(conversation)),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

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
    })
}

/// A random 32-byte bearer token, hex-encoded: two ULIDs give 128 random
/// bits each with millisecond ordering, plenty for a per-process secret.
fn bearer_token() -> String {
    use ulid::Ulid;
    format!("{}{}", Ulid::new(), Ulid::new()).to_lowercase()
}

/// Stand-in used when the metacognition layer could not be built: every
/// remark call errors, which surfaces as `navigator/status` error instead
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
                    "metacognition": metacognition,
                    "reflex": { "provider": "omni", "model": "m-fim", "api": "fim" },
                    "instinct": { "provider": "omni", "model": "m-s1", "api": "systemone" }
                }
            })
            .to_string(),
        )
        .expect("config")
    }

    async fn layers(models: &ModelsConfig) -> (tempfile::TempDir, Layers) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("cfg");
        auth::set_key(&cfg, "omni", "test-key").expect("key");
        let layers = build_layers_from(
            models,
            &cfg,
            &ModelCatalog::new(Some(dir.path().join("data"))),
            dir.path().join("log.jsonl"),
            Arc::new(ClientCache::new()),
        )
        .await;
        (dir, layers)
    }

    // Given a metacognition alias with no window and a provider that cannot
    //       be reached
    // When the layers are built
    // Then only the remarks are disabled, once, with the reason; FIM and the
    //      instinct stay up, and the stand-in answers with an error
    #[tokio::test]
    async fn an_unresolved_metacognition_disables_only_the_remarks() {
        let (_dir, layers) = layers(&config(None)).await;
        assert!(layers.metacognition.is_none());
        assert!(layers.metacognition_model.is_none());
        assert!(layers.fim.is_some(), "FIM is not touched");
        assert!(layers.decision.is_some(), "the instinct is not touched");
        assert_eq!(layers.disabled.len(), 1, "{:?}", layers.disabled);
        assert!(
            layers.disabled[0].starts_with("metacognition:")
                && layers.disabled[0].contains("no known context window")
                && layers.disabled[0].contains("'metacognition'"),
            "{:?}",
            layers.disabled
        );
        let ctx = RemarkContext::build("file:///a.rs", "rust", "a", "b", Vec::new(), None);
        let cancel = tokio_util::sync::CancellationToken::new();
        assert!(layers.metacognition().remark(&ctx, &cancel).await.is_err());
    }

    // Given the same alias with its own window
    // When the layers are built
    // Then the commenter is built without any lookup and nothing is disabled
    #[tokio::test]
    async fn a_metacognition_with_a_window_is_built() {
        let (_dir, layers) = layers(&config(Some(128_000))).await;
        assert!(layers.metacognition.is_some());
        assert_eq!(layers.metacognition_model.as_deref(), Some("m-chat"));
        assert!(layers.fim.is_some() && layers.decision.is_some());
        assert!(layers.disabled.is_empty(), "{:?}", layers.disabled);
    }
}
