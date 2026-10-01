//! Process wiring for the `rust-agent-lsp` binary: builds the model layers
//! from models.json and auth.json, starts the in-process cognition chat
//! server, assembles the [`NavigatorDeps`] and serves the Language Server
//! over stdio. A layer whose alias, key or config is missing is disabled
//! with a note and the others keep working.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::navigator::log::NavLog;
use crate::navigator::metacognition::LlmCommenter;
use crate::navigator::ports::{DecisionModel, FimModel};
use crate::navigator::{
    self, ConversationFeed, Metacognition, ModelError, NavigatorDeps, Remark, RemarkContext,
    ServerInfoParams,
};
use crate::provider::auth;
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
    let models_path = navigator::resolve_models(explicit, &workspace)
        .or_else(|| Some(config_dir.join("models.json")))
        .ok_or_else(|| anyhow::anyhow!("no models.json found"))?;
    ModelsConfig::load(&models_path)
        .map_err(|e| anyhow::anyhow!("cannot load {}: {e}", models_path.display()))
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
fn build_layers(models_path: Option<&Path>, clients: Arc<ClientCache>) -> Layers {
    let mut layers = Layers {
        fim: None,
        decision: None,
        reflex_model: None,
        instinct_model: None,
        metacognition_model: None,
        metacognition: None,
        disabled: Vec::new(),
    };
    let models = match load_models(models_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("models.json unavailable, every model layer disabled: {e}");
            layers.disabled.push(format!("all: models.json ({e})"));
            return layers;
        }
    };
    let config_dir = auth::default_config_dir().unwrap_or_else(|| PathBuf::from("."));

    // Reflex: Mercury FIM on the `reflex` alias.
    match models.resolve("reflex") {
        Ok(resolved) => {
            let model_id = resolved.model.model.clone();
            match auth::resolve_key(
                &config_dir,
                &resolved.provider_name,
                resolved.provider.api_key_env.as_deref(),
            ) {
                Ok(key) => {
                    layers.fim = Some(Arc::new(InceptionFim::new(resolved, key, clients.clone())));
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
    match SystemOneClient::from_alias(&models, "instinct", &config_dir, clients.clone()) {
        Ok(client) => {
            layers.instinct_model = Some(client.model().to_string());
            layers.decision = Some(Arc::new(client));
        }
        Err(e) => {
            tracing::error!("instinct disabled: {e}");
            layers.disabled.push(format!("instinct: {e}"));
        }
    }

    // Metacognition: the fast chat commenter on the `metacognition` alias,
    // logging to the navigator log.
    let metacognition_log = navigator::default_log_path();
    match build_llm_commenter(
        &models,
        "metacognition",
        &config_dir,
        clients.clone(),
        metacognition_log.unwrap_or_else(|| PathBuf::from("/dev/null")),
    ) {
        Ok(commenter) => {
            layers.metacognition_model = Some(commenter.model_id().to_string());
            layers.metacognition = Some(Arc::new(commenter));
        }
        Err(e) => {
            tracing::error!("metacognition disabled: {e}");
            layers.disabled.push(format!("metacognition: {e}"));
        }
    }

    layers
}

/// Chat commenter on a named alias (the metacognition).
fn build_llm_commenter(
    models: &ModelsConfig,
    alias: &str,
    config_dir: &Path,
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
    let layers = build_layers(models_path, clients);

    let log_path = navigator::default_log_path()
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
            // A failed bind must not take the navigator down: run the LSP
            // without the chat (no serverInfo notification).
            tracing::error!("cognition chat server unavailable: {e}");
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
    // Conversation history: one SQLite file in the navigator's data dir.
    let history_db = directories::ProjectDirs::from("", "", "rust-agent")
        .map(|dirs| dirs.data_dir().join("navigator").join("sessions.db"));
    if let Some(path) = &history_db {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path,
        config_dir,
        allow_non_loopback: false,
        bearer_token: Some(token.clone()),
        history_db,
    };
    let state = AppState::with_conversation(config, Some(conversation));
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
