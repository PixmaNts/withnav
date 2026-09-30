use clap::Parser;
use rust_agent::app::{dispatch, init_tracing, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let code = dispatch(cli).await?;
    std::process::exit(code);
}


