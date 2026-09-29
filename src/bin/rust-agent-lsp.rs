//! `rust-agent-lsp`: the navigator Language Server over stdio.
//!
//! stdout is the LSP channel; all logging goes to stderr via tracing.
//! Each layer (reflex, instinct, metacognition) is built from its alias in
//! models.json; a layer whose alias/key/config is missing is disabled with a
//! status message and the others keep working.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rust_agent::navigator;
use rust_agent::navigator::commenter::LlmCommenter;
use rust_agent::navigator::{Commenter, CommenterAsMetacognition, Metacognition};
use rust_agent::provider::auth;
use rust_agent::provider::config::ModelsConfig;
use rust_agent::provider::fim::InceptionFim;
use rust_agent::provider::openai::ClientCache;
use rust_agent::provider::systemone::SystemOneClient;

#[derive(Parser, Debug)]
#[command(
    name = "rust-agent-lsp",
    version,
    about = "Navigator: a pair-programming navigator over LSP"
)]
struct Cli {
    /// Explicit path to models.json (default: workspace, then config dir).
    #[arg(long)]
    models: Option<PathBuf>,
    /// Accepted and ignored: stdio is the only transport. vscode-languageclient
    /// appends this flag when launched with `TransportKind.stdio`.
    #[arg(long, hide = true)]
    stdio: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Replays a candidate pipeline over the recorded trigger log and
    /// compares its decisions with the ones the driver took.
    Replay {
        /// A `.navigator` directory or its `pipeline.toml` (default: the
        /// built-in pipeline).
        #[arg(long)]
        pipeline: Option<PathBuf>,
        /// Decision log to replay (default: the navigator log).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Re-render the state and ask the instinct again when the candidate's
        /// question differs from the logged one. Without it such a trigger is
        /// reported as not comparable.
        #[arg(long)]
        reask: bool,
        /// Machine-readable report on stdout instead of the text table.
        #[arg(long)]
        json: bool,
    },
}

fn init_tracing_stderr() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // stdout is the LSP channel: logs MUST go to stderr only.
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Resolves the models config: explicit path, workspace copy (it carries the
/// navigator aliases), then the config dir.
fn load_models(cli: &Cli) -> anyhow::Result<ModelsConfig> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let models_path = navigator::resolve_models(cli.models.as_deref(), &workspace)
        .or_else(|| Some(config_dir.join("models.json")))
        .ok_or_else(|| anyhow::anyhow!("no models.json found"))?;
    ModelsConfig::load(&models_path)
        .map_err(|e| anyhow::anyhow!("cannot load {}: {e}", models_path.display()))
}

/// The metacognition over the LLM commenter; the pipeline prompt is carried
/// on the `RemarkContext`.
struct PromptedCommenter {
    inner: Arc<LlmCommenter>,
}

impl Metacognition for PromptedCommenter {
    fn remark<'a>(
        &'a self,
        ctx: &'a navigator::RemarkContext,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<navigator::Remark>, String>> + Send + 'a,
        >,
    > {
        self.inner.remark(ctx, cancel)
    }
}

struct Layers {
    fim: Option<Arc<dyn rust_agent::navigator::ports::FimModel>>,
    decision: Option<Arc<dyn rust_agent::navigator::ports::DecisionModel>>,
    reflex_model: Option<String>,
    instinct_model: Option<String>,
    metacognition_model: Option<String>,
    metacognition: Option<Arc<LlmCommenter>>,
    disabled: Vec<String>,
}

/// Builds every layer from its alias. A missing alias or key disables that
/// layer with a note; the others keep working.
fn build_layers(cli: &Cli, clients: Arc<ClientCache>) -> Layers {
    let mut layers = Layers {
        fim: None,
        decision: None,
        reflex_model: None,
        instinct_model: None,
        metacognition_model: None,
        metacognition: None,
        disabled: Vec::new(),
    };
    let models = match load_models(cli) {
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

    // Metacognition: the fast chat commenter on the `metacognition` alias.
    match build_llm_commenter(&models, "metacognition", &config_dir, clients.clone()) {
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
    config_dir: &std::path::Path,
    clients: Arc<ClientCache>,
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
    Ok(LlmCommenter::new(resolved, api_key, clients))
}

impl Layers {
    /// The metacognition seam: the real commenter when built, else a
    /// failing stand-in so the editor never crashes.
    fn metacognition(&self) -> Arc<dyn Metacognition> {
        match &self.metacognition {
            Some(llm) => Arc::new(PromptedCommenter {
                inner: Arc::clone(llm),
            }),
            None => Arc::new(CommenterAsMetacognition(Arc::new(FailingCommenter {
                message: "metacognition layer disabled".to_string(),
            }))),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing_stderr();
    let cli = Cli::parse();

    if let Some(Command::Replay {
        ref pipeline,
        ref log,
        reask,
        json,
    }) = cli.command
    {
        return run_replay(&cli, pipeline.as_deref(), log.as_deref(), reask, json).await;
    }

    let clients = Arc::new(ClientCache::new());
    let layers = build_layers(&cli, clients);

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
    let http = match spawn_cognition_server(&cli).await {
        Ok(http) => Some(http),
        Err(e) => {
            // A failed bind must not take the navigator down: run the LSP
            // without the chat (no serverInfo notification).
            tracing::error!("cognition chat server unavailable: {e}");
            None
        }
    };

    let mut deps = navigator::NavigatorDeps::with_layers(layers.metacognition(), log_path)
        .with_dirs(workspace, user_dir)
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
    info: navigator::ServerInfoParams,
}

/// The in-process cognition server: an [`AppState`] over the same models
/// resolution as the LSP layers, bound to a loopback ephemeral port, with a
/// random 64-hex-char bearer token.
async fn spawn_cognition_server(cli: &Cli) -> anyhow::Result<CognitionHttp> {
    use rust_agent::server::session::{AppState, ServerConfig};

    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let models_path = navigator::resolve_models(cli.models.as_deref(), &workspace);
    let config_dir = auth::default_config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let token = bearer_token();
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
    let state = AppState::new(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = rust_agent::server::routes::serve_until(
            state,
            listener,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
    });

    Ok(CognitionHttp {
        task,
        shutdown: shutdown_tx,
        info: navigator::ServerInfoParams { port, token },
    })
}

/// A random 32-byte bearer token, hex-encoded: two ULIDs give 128 random
/// bits each with millisecond ordering, plenty for a per-process secret.
fn bearer_token() -> String {
    use ulid::Ulid;
    format!("{}{}", Ulid::new(), Ulid::new()).to_lowercase()
}

/// Replays a candidate pipeline over the recorded trigger log. Nothing here
/// talks to the editor: the instinct model is only built when `--reask` was
/// asked for, and then only for the triggers whose question changed.
async fn run_replay(
    cli: &Cli,
    pipeline_path: Option<&std::path::Path>,
    log_path: Option<&std::path::Path>,
    reask: bool,
    json: bool,
) -> anyhow::Result<()> {
    use rust_agent::navigator::pipeline::Pipeline;
    use rust_agent::navigator::replay::{load_candidate, ReplayLog, Replayer};

    let candidate = load_candidate(pipeline_path)
        .map_err(|e| anyhow::anyhow!("cannot load the candidate pipeline: {e}"))?;
    let pipeline = Pipeline::new(candidate);

    let log_path = match log_path {
        Some(path) => path.to_path_buf(),
        None => navigator::default_log_path()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve the user data dir; pass --log"))?,
    };
    // A missing log replays as an empty one: no triggers, nothing to compare.
    let log = match ReplayLog::read(&log_path) {
        Ok(log) => log,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::anyhow!("no such log: {}", log_path.display()));
        }
        Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", log_path.display())),
    };

    let report = if reask {
        let models = load_models(cli)?;
        let config_dir = auth::default_config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir for the instinct API key"))?;
        let alias = pipeline.snapshot().model_alias().to_string();
        let client =
            SystemOneClient::from_alias(&models, &alias, &config_dir, Arc::new(ClientCache::new()))
                .map_err(|e| {
                    anyhow::anyhow!("cannot build the instinct model on '{alias}': {e}")
                })?;
        Replayer::with_reask(&pipeline, &client).run(&log).await
    } else {
        Replayer::new(&pipeline).run(&log).await
    };

    // The snapshot only knows builtin/user/project: report the path the driver
    // actually pointed at.
    let report = match pipeline_path {
        Some(path) => report.with_pipeline_origin(&path.display().to_string()),
        None => report.with_pipeline_origin("built-in"),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| anyhow::anyhow!("{e}"))?
        );
    } else {
        print!(
            "{}",
            report.render_text(navigator::replay::DEFAULT_LIST_LIMIT)
        );
    }
    Ok(())
}

/// Stand-in used when the metacognition layer could not be built: every
/// remark call errors, which surfaces as `navigator/status` error instead
/// of a crash.
struct FailingCommenter {
    message: String,
}

impl Commenter for FailingCommenter {
    fn remark<'a>(
        &'a self,
        _ctx: &'a navigator::RemarkContext,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<navigator::Remark>, String>> + Send + 'a,
        >,
    > {
        let msg = self.message.clone();
        Box::pin(async move { Err(msg) })
    }
}
