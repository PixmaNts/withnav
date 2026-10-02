use clap::Parser;
use navai::app::{dispatch, init_tracing, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
