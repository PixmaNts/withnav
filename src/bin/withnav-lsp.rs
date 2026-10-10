//! `withnav-lsp`: the navigator Language Server over stdio.
//!
//! stdout is the LSP channel; all logging goes to stderr via tracing. This
//! file only parses the command line and dispatches: the server wiring lives
//! in `navigator::bootstrap`, the replay report in `navigator::replay`.

use std::path::PathBuf;

use clap::Parser;
use withnav::navigator;
use withnav::navigator::bootstrap;

#[derive(Parser, Debug)]
#[command(
    name = "withnav-lsp",
    version,
    about = "WithNav language server: Reflex, Instinct and Metacognition in your editor"
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
    /// Replays built-in editor flows offline against captured decisions.
    Replay {
        /// Accepted for compatibility; flows are the only replay mode.
        #[arg(long)]
        flows: bool,
        /// Decision log to replay (default: the navigator log).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Machine-readable report instead of a summary.
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
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> anyhow::Result<()> {
    withnav::tools::bash::init_shell();
    // The editor host runs the agent too, and its `fetch` converts pages in
    // a child started from this same executable: handle that invocation
    // before the async runtime exists (see `conversion_child`).
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = withnav::tools::fetch::conversion_child(&args) {
        std::process::exit(code);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    init_tracing_stderr();
    let cli = Cli::parse();

    if let Some(Command::Replay { log, json, .. }) = cli.command {
        let path = log
            .or_else(withnav::paths::decision_log)
            .ok_or_else(|| anyhow::anyhow!("pass --log"))?;
        let report = navigator::replay::flows::report(&std::fs::read_to_string(path)?);
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{} compared, {} changed, {} not comparable",
                report["compared"],
                report["changed"].as_array().map_or(0, Vec::len),
                report["not_comparable"].as_array().map_or(0, Vec::len)
            );
        }
        return Ok(());
    }
    bootstrap::run(cli.models.as_deref()).await
}
