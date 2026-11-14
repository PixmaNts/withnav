pub mod entry;
use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::agent::instructions::RulesRoots;
use crate::bus::event::{AgentYield, EventEnvelope, EventKind, LlmDelta, TurnEnd, TurnEndOutcome};
use crate::provider::auth;
use crate::render::{Renderer, UiEvent};

pub const EXIT_DONE: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_YIELDED: i32 = 2;
pub const EXIT_INTERRUPTED: i32 = 130;
/// A tool call needs approval and nobody can give it: `nav run` never
/// approves on its own.
pub const EXIT_PERMISSION_REQUIRED: i32 = 3;

#[derive(Parser, Debug)]
#[command(
    name = "nav",
    version = env!("WITHNAV_VERSION"),
    about = "WithNav: you set the course, Nav lights the way. A pair-programming crew for your code",
    propagate_version = true,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Without a subcommand, `nav` opens the terminal client with these flags.
    #[command(flatten)]
    pub tui: TuiArgs,
}

impl Cli {
    /// The command to run: the subcommand given, else the terminal client.
    pub fn into_command(self) -> Commands {
        self.command.unwrap_or(Commands::Tui(self.tui))
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// WithNav language server: Reflex, Instinct and Metacognition in your editor.
    Lsp(lsp::LspArgs),
    /// Read-only diagnostics. Live checks send small requests and may incur provider charges.
    Doctor(DoctorArgs),
    /// Inspect the host flow schema.
    Flows {
        #[command(subcommand)]
        command: FlowCommand,
    },
    /// Manage permission rules and inspect decision history.
    Permissions {
        #[command(subcommand)]
        command: PermissionCommand,
    },
    /// Run one task and exit.
    Run(RunArgs),
    /// The session server alone (HTTP and SSE).
    Serve(ServeArgs),
    /// Store API keys.
    Auth(AuthArgs),
    /// Terminal chat client; what `nav` runs when no command is given.
    Tui(TuiArgs),
    /// Serves the browser client of the conversation and opens it.
    Web(WebArgs),
    /// Converts HTML from stdin to text on stdout; run by the `fetch` tool in
    /// a resource-limited child process.
    #[command(name = "internal-html-convert", hide = true)]
    InternalHtmlConvert(HtmlConvertArgs),
}

#[derive(Subcommand, Debug)]
pub enum FlowCommand {
    Catalogue,
}

#[derive(Args, Debug, Default)]
#[group(multiple = false)]
pub struct PermissionScopeArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub global: bool,
    #[arg(long)]
    pub agent: Option<String>,
}

impl PermissionScopeArgs {
    fn scope(self) -> Option<crate::permission::rules::RuleScope> {
        use crate::permission::rules::RuleScope;
        if self.global {
            Some(RuleScope::Global)
        } else if let Some(project) = self.project {
            Some(RuleScope::Project(project))
        } else {
            self.agent.map(RuleScope::Agent)
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum PermissionCommand {
    Validate {
        #[arg(value_enum)]
        kind: crate::permission::validation::ValidationKind,
        name: String,
        #[arg(long, value_enum)]
        location: Option<crate::flows::store::Location>,
    },
    Validations {
        #[arg(long)]
        json: bool,
    },
    RevokeValidation {
        id: i64,
    },
    List {
        /// Include revoked rules.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        scope: PermissionScopeArgs,
        #[arg(long)]
        json: bool,
    },
    Add {
        action: String,
        resource: String,
        #[arg(value_enum)]
        effect: crate::permission::rules::Effect,
        /// Defaults to the current project.
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    Revoke {
        id: i64,
    },
    History {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct HtmlConvertArgs {
    #[arg(long, value_enum)]
    pub format: crate::tools::fetch::ConvertFormat,
}

pub mod connect;
pub mod doctor;
mod embedded;
mod permissions;
mod run;
mod serve;
use permissions::permissions_command;
#[cfg(test)]
use run::read_prompt;
#[cfg(test)]
use run::render_until_terminal;
use run::run_command;
pub use run::{event_to_ui, exit_code_for, terminal_stderr_line};
use serve::{history_db_path, serve_command};
pub mod logging;
pub mod lsp;
pub mod tui;
pub mod web;

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Make no network requests; use only local configuration and cached metadata.
    #[arg(long)]
    pub offline: bool,
    /// Emit one machine-readable report instead of human output.
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_default())]
    pub workspace: PathBuf,
    #[arg(long, value_enum)]
    pub layer: Option<doctor::DoctorLayer>,
}

/// Logging for `tui`: a file in the data dir, never the terminal.
pub fn init_tui_logging() {
    tui::init_logging();
}

#[derive(Args, Debug)]
pub struct TuiArgs {
    /// Workspace directory the conversation works in.
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_default())]
    pub workspace: PathBuf,
    /// Model alias for a new conversation.
    #[arg(long, conflicts_with = "session")]
    pub model: Option<String>,
    /// Id of a past conversation to resume.
    #[arg(long, conflicts_with = "model")]
    pub session: Option<String>,
    /// Path of the models file: the background owner must have been started with the same one.
    #[arg(long, conflicts_with = "url")]
    pub models: Option<PathBuf>,
    /// Connect to a running server instead of joining the background owner. If it needs a
    /// bearer token, set WITHNAV_SERVER_TOKEN.
    #[arg(long, conflicts_with = "models")]
    pub url: Option<String>,
}

#[derive(Args, Debug)]
pub struct WebArgs {
    /// A workspace directory to offer in the browser; repeat for several. Defaults to the current
    /// directory.
    #[arg(long = "workspace", value_name = "PATH")]
    pub workspaces: Vec<PathBuf>,
    /// Path of the models file to require from the background server.
    #[arg(long)]
    pub models: Option<PathBuf>,
    /// Refused: the background server picks its port (`nav serve --port` sets it).
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// Print the address without opening the browser.
    #[arg(long)]
    pub no_open: bool,
}

#[derive(Args, Debug, Default)]
pub struct ServeArgs {
    /// Loopback port to listen on; 0 picks a free one.
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    #[arg(long)]
    pub models: Option<PathBuf>,

    /// Run the server detached in the background.
    #[arg(long, hide = true)]
    pub detach: bool,

    #[command(subcommand)]
    pub command: Option<ServeSubcommand>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ServeSubcommand {
    /// Stop the running owner server.
    Stop {
        /// Force stop even if turns are running.
        #[arg(long)]
        force: bool,
    },
    /// Print the status of the owner server.
    Status,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(short, long)]
    pub model: Option<String>,

    #[arg(short, long)]
    pub prompt: Option<String>,

    #[arg(long)]
    pub workspace: Option<PathBuf>,

    /// Explicit models.json; the background owner must have been started with the same one.
    #[arg(long)]
    pub models: Option<PathBuf>,

    #[arg(long)]
    pub show_thinking: bool,

    /// The primary agent to run as (`build` by default; `plan` researches and
    /// writes markdown plans).
    #[arg(long)]
    pub agent: Option<String>,
    /// Model variant to run with: the exact name of one the alias offers, or
    /// `off` for no variant. Default: the alias's default variant.
    #[arg(long)]
    pub variant: Option<String>,
    /// UNSAFE: turn on bypass for the run, so ordinary permission prompts
    /// (`write`, `edit`, `bash`, `fetch`) run without asking. Protected
    /// requests (WithNav's own config, credentials and keys) still stop the run
    /// with `permission_required` and exit code 3, as every request does
    /// without this flag.
    #[arg(long)]
    pub yes: bool,
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
    Remove { provider: String },
}

pub fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(logging::mcp_safe_filter())
        .with(
            fmt::layer()
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())),
        )
        .try_init();
}

pub async fn dispatch(command: Commands) -> anyhow::Result<i32> {
    match command {
        Commands::Doctor(args) => doctor::command(args).await,
        Commands::Permissions { command } => permissions_command(command).await,
        Commands::Flows {
            command: FlowCommand::Catalogue,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::flows::catalogue::catalogue())?
            );
            Ok(EXIT_DONE)
        }
        Commands::Run(args) => run_command(args).await,
        Commands::Lsp(args) => lsp::run(args).await,
        Commands::Serve(args) => serve_command(args).await,
        Commands::Auth(args) => auth_command(args),
        Commands::Tui(args) => tui::run(args).await,
        Commands::Web(args) => web::run(args).await,
        Commands::InternalHtmlConvert(args) => Ok(crate::tools::fetch::convert_stdio(args.format)),
    }
}

fn config_dir() -> anyhow::Result<PathBuf> {
    auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))
}

fn auth_command(args: AuthArgs) -> anyhow::Result<i32> {
    let dir = config_dir()?;
    match args.command {
        AuthCommand::Set { provider } => {
            let key = rpassword_prompt(&provider)?;
            auth::set_key(&dir, &provider, &key)?;
            println!("stored key for provider '{provider}' (0600)");
            Ok(EXIT_DONE)
        }
        AuthCommand::List => {
            let providers = auth::list_providers(&dir)?;
            if providers.is_empty() {
                println!("(no providers configured)");
            } else {
                for p in providers {
                    println!("{p}");
                }
            }
            Ok(EXIT_DONE)
        }
        AuthCommand::Remove { provider } => {
            if auth::remove_key(&dir, &provider).is_ok() {
                println!("removed key for provider '{provider}'");
            } else {
                println!("no key found for provider '{provider}'");
            }
            Ok(EXIT_DONE)
        }
    }
}

fn rpassword_prompt(provider: &str) -> anyhow::Result<String> {
    eprint!("API key for '{provider}' (input hidden not supported; reads a line from stdin): ");
    use std::io::Write;
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests;
