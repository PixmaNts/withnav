use clap::Parser;
use navai::app::{dispatch, init_tracing, Cli};

fn main() -> anyhow::Result<()> {
    // A conversion child touches no data file, logs nothing, and starts no
    // async runtime: it runs under a memory limit set by its parent.
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
    let cli = Cli::parse();
    // Before anything opens a data file or the log: bring over the former
    // locations. The log is not set up yet, so the report is written after.
    let migration = navai::paths::migrate_legacy();
    if !matches!(cli.command, navai::app::Commands::Tui(_)) {
        init_tracing();
    } else {
        navai::app::init_tui_logging();
    }
    migration.log();
    let code = dispatch(cli).await?;
    std::process::exit(code);
}
