use clap::Parser;
use rust_agent::app::{dispatch, init_tracing, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.command, rust_agent::app::Commands::Tui(_)) {
        init_tracing();
    } else {
        rust_agent::app::init_tui_logging();
    }
    let code = dispatch(cli).await?;
    std::process::exit(code);
}
