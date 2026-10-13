//! The editor server and offline replay entry points for `nav lsp`.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct LspArgs {
    /// Explicit path to models.json (default: workspace, then config dir).
    #[arg(long)]
    pub models: Option<PathBuf>,
    /// Accepted and ignored: vscode-languageclient appends this flag, and
    /// stdio is the editor server's only transport.
    #[arg(long, hide = true)]
    pub stdio: bool,
    #[command(subcommand)]
    pub command: Option<LspCommand>,
}

#[derive(Subcommand, Debug)]
pub enum LspCommand {
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

pub fn init_tracing_stderr() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // stdout carries LSP frames; tracing must never write into that channel.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(super::logging::mcp_safe_filter())
        .with(
            fmt::layer()
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_writer(std::io::stderr),
        )
        .try_init();
}

pub async fn run(args: LspArgs) -> anyhow::Result<i32> {
    if let Some(LspCommand::Replay { log, json, .. }) = args.command {
        let path = log
            .or_else(crate::paths::decision_log)
            .ok_or_else(|| anyhow::anyhow!("pass --log"))?;
        let report = crate::navigator::replay::flows::report(&std::fs::read_to_string(path)?);
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
    } else {
        crate::navigator::bootstrap::run(args.models.as_deref()).await?;
    }
    Ok(super::EXIT_DONE)
}
