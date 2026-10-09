use crate::app::{dispatch, init_tracing, Cli};
use clap::Parser;

pub fn main() -> anyhow::Result<()> {
    crate::tools::bash::init_shell();
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
    match &cli.command {
        crate::app::Commands::Lsp(_) => crate::app::lsp::init_tracing_stderr(),
        crate::app::Commands::Tui(_) => crate::app::init_tui_logging(),
        // Provider bodies and URLs can contain secrets. Doctor deliberately has
        // no tracing subscriber, even when RUST_LOG requests wire diagnostics.
        crate::app::Commands::Doctor(_) => {}
        _ => init_tracing(),
    }
    let code = dispatch(cli).await?;
    std::process::exit(code);
}
