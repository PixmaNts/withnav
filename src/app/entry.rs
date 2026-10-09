use crate::app::{dispatch, init_tracing, Cli};
use clap::Parser;

pub fn main() -> anyhow::Result<()> {
    // A conversion child touches no data file, logs nothing, and starts no
    // async runtime: it runs under a memory limit set by its parent.
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = crate::tools::fetch::conversion_child(&args) {
        std::process::exit(code);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.command, crate::app::Commands::Tui(_)) {
        init_tracing();
    } else {
        crate::app::init_tui_logging();
    }
    let code = dispatch(cli).await?;
    std::process::exit(code);
}
