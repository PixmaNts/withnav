//! `rust-agent-lsp`: the navigator Language Server over stdio.
//!
//! stdout is the LSP channel; all logging goes to stderr via tracing.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rust_agent::navigator;
use rust_agent::navigator::commenter::{Commenter, LlmCommenter};
use rust_agent::provider::auth;
use rust_agent::provider::config::ModelsConfig;
use rust_agent::provider::openai::ClientCache;

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

fn build_llm_commenter(cli: &Cli) -> anyhow::Result<LlmCommenter> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir =
        auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?;
    let models_path = navigator::resolve_models(cli.models.as_deref(), &workspace)
        .or_else(|| Some(config_dir.join("models.json")))
        .ok_or_else(|| anyhow::anyhow!("no models.json found"))?;
    let models = ModelsConfig::load(&models_path)
        .map_err(|e| anyhow::anyhow!("cannot load {}: {e}", models_path.display()))?;

    // Default alias from config `fast`, overridable later via LSP config.
    let alias = "fast";
    let resolved = models
        .resolve(alias)
        .map_err(|e| anyhow::anyhow!("cannot resolve alias '{alias}': {e}"))?;
    let api_key = auth::resolve_key(
        &config_dir,
        &resolved.provider_name,
        resolved.provider.api_key_env.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("no API key for provider '{}': {e}", resolved.provider_name))?;
    Ok(LlmCommenter::new(resolved, api_key, Arc::new(ClientCache::new())))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing_stderr();
    let cli = Cli::parse();

    let commenter: Arc<dyn Commenter> = match build_llm_commenter(&cli) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            // Never crash the editor: run with an always-silent commenter and
            // surface the failure in logs. The status notification only
            // reaches the client after initialize, so tracing is the channel.
            tracing::error!("navigator LLM setup failed, staying silent: {e}");
            Arc::new(FailingCommenter {
                message: format!("setup failed: {e}"),
            })
        }
    };

    let log_path = navigator::default_log_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve user data dir"))?;
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let deps = navigator::NavigatorDeps::new(commenter, log_path);
    navigator::serve(tokio::io::stdin(), tokio::io::stdout(), deps).await;
    Ok(())
}

/// Stand-in used when model/auth setup fails: every remark call errors, which
/// surfaces as `navigator/status` error instead of a crash.
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
