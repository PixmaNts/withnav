use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rust-agent",
    version,
    about = "A personal, minimal-but-complete coding agent in Rust",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Run(RunArgs),
    Serve,
    Tui,
    Auth(AuthArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(short, long)]
    pub model: Option<String>,

    #[arg(short, long)]
    pub prompt: Option<String>,

    #[arg(long)]
    pub workspace: Option<PathBuf>,

    #[arg(long)]
    pub models: Option<PathBuf>,

    #[arg(long)]
    pub show_thinking: bool,
}

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    Set { provider: String },
    List,
}

pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Commands::Run(_args) => {
            println!("run: not yet wired (Phase 1 todo 12)");
            Ok(0)
        }
        Commands::Serve => {
            println!("not yet implemented (Phase 2/4)");
            Ok(0)
        }
        Commands::Tui => {
            println!("not yet implemented (Phase 2/4)");
            Ok(0)
        }
        Commands::Auth(_args) => {
            println!("auth: not yet wired (Phase 1 todo 12)");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_debug_assert() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_lists_four_subcommands() {
        let cmd = Cli::command();
        let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        for expected in ["run", "serve", "tui", "auth"] {
            assert!(names.contains(&expected), "missing subcommand: {expected}");
        }
    }

    #[test]
    fn bogus_subcommand_is_error() {
        let result = Cli::try_parse_from(["rust-agent", "bogus-subcommand"]);
        assert!(result.is_err());
    }
}
