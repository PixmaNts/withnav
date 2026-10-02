use clap::Parser;
use navai::app::{dispatch, init_tracing, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.command, navai::app::Commands::Tui(_)) {
        init_tracing();
    } else {
        navai::app::init_tui_logging();
    }
    let code = dispatch(cli).await?;
    std::process::exit(code);
}
