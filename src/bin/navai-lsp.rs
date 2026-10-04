//! `navai-lsp`: the navigator Language Server over stdio.
//!
//! stdout is the LSP channel; all logging goes to stderr via tracing. This
//! file only parses the command line and dispatches: the server wiring lives
//! in `navigator::bootstrap`, the replay report in `navigator::replay`.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use navai::navigator;
use navai::navigator::bootstrap;
use navai::navigator::models::DecisionAdapter;
use navai::provider::auth;
use navai::provider::openai::ClientCache;

#[derive(Parser, Debug)]
#[command(
    name = "navai-lsp",
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

fn main() -> anyhow::Result<()> {
    // The editor host runs the agent too, and its `fetch` converts pages in
    // a child started from this same executable: handle that invocation
    // before the async runtime exists (see `conversion_child`).
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = navai::tools::fetch::conversion_child(&args) {
        std::process::exit(code);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let migration = navai::paths::migrate_legacy();
    init_tracing_stderr();
    migration.log();
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

    bootstrap::run(cli.models.as_deref()).await
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
    use navai::navigator::pipeline::Pipeline;
    use navai::navigator::replay::{load_candidate, ReplayLog, Replayer};

    let candidate = load_candidate(pipeline_path)
        .map_err(|e| anyhow::anyhow!("cannot load the candidate pipeline: {e}"))?;
    let pipeline = Pipeline::new(candidate);

    let log_path = match log_path {
        Some(path) => path.to_path_buf(),
        None => navai::paths::decision_log()
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
        let models = bootstrap::load_models(cli.models.as_deref())?;
        let config_dir = auth::default_config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir for the instinct API key"))?;
        let client =
            navai::provider::layers::instinct(&models, &config_dir, Arc::new(ClientCache::new()))?;
        let decision = DecisionAdapter(client);
        Replayer::with_reask(&pipeline, &decision).run(&log).await
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
