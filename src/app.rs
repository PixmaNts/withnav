pub mod entry;
use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

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
    version,
    about = "WithNav: you set the course, Nav lights the way. A pair-programming crew for your code",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
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
    Run(RunArgs),
    Serve(ServeArgs),
    Auth(AuthArgs),
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

fn permissions_command(command: PermissionCommand) -> anyhow::Result<i32> {
    use crate::permission::{
        decisions::DecisionFilter,
        rules::{RuleFilter, RuleOrigin, RuleScope},
        store::PermissionStore,
    };
    let data_dir = crate::paths::data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))?;
    let store = PermissionStore::open(&data_dir)?;
    match command {
        PermissionCommand::Validate {
            kind,
            name,
            location,
        } => {
            if kind != crate::permission::validation::ValidationKind::Flow {
                anyhow::bail!("skill admission is not available yet");
            }
            let flows = crate::flows::store::FlowStore::with_data(
                std::env::current_dir()?,
                config_dir()?,
                data_dir,
            );
            let location = match location {
                Some(location) => location,
                None => {
                    let matches: Vec<_> = flows
                        .list()
                        .map_err(|e| anyhow::anyhow!("{}", e.reason))?
                        .into_iter()
                        .filter(|flow| {
                            flow.name == name
                                && flow.location != crate::flows::store::Location::Builtin
                        })
                        .collect();
                    if matches.len() != 1 {
                        anyhow::bail!("select --location global or workspace for an ambiguous or missing flow");
                    }
                    matches[0].location
                }
            };
            let document = flows
                .get(&name, location)
                .map_err(|e| anyhow::anyhow!("{}", e.reason))?;
            let approval = flows
                .validate_version(&name, location, &document.revision)
                .map_err(|e| anyhow::anyhow!("{}", e.reason))?;
            println!(
                "validated flow {} at {} ({}) as {}",
                name, approval.location, approval.content_sha256, approval.id
            );
        }
        PermissionCommand::Validations { json } => {
            let validations = store.list_validations()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&validations)?);
            } else {
                for validation in validations {
                    println!(
                        "{}\t{}\t{}\t{}\t{}{}",
                        validation.id,
                        validation.kind.as_str(),
                        validation.name,
                        validation.location,
                        validation.content_sha256,
                        if validation.revoked_at.is_some() {
                            "\trevoked"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        PermissionCommand::RevokeValidation { id } => {
            if !store.revoke_validation(id)? {
                anyhow::bail!("validation {id} not found or already revoked");
            }
            println!("revoked validation {id}; reload flows to apply");
        }
        PermissionCommand::List { all, scope, json } => {
            let rules = store.list(&RuleFilter {
                include_revoked: all,
                scope: scope.scope(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rules)?);
            } else if rules.is_empty() {
                println!("(no permission rules)");
            } else {
                for rule in rules {
                    println!(
                        "{}\t{}\t{}\t{}\t{:?}{}",
                        rule.id,
                        rule.action,
                        rule.resource,
                        rule.effect.as_str(),
                        rule.scope,
                        if rule.revoked_at.is_some() {
                            "\trevoked"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        PermissionCommand::Add {
            action,
            resource,
            effect,
            scope,
        } => {
            let scope = match scope.scope() {
                Some(scope) => scope,
                None => RuleScope::Project(std::env::current_dir()?),
            };
            let rule = store.add(&action, &resource, effect, scope, RuleOrigin::Manual)?;
            println!("added permission rule {}", rule.id);
        }
        PermissionCommand::Revoke { id } => {
            if !store.revoke(id)? {
                anyhow::bail!("permission rule {id} not found or already revoked");
            }
            println!("revoked permission rule {id}");
        }
        PermissionCommand::History { limit, json } => {
            let decisions = store.list_decisions(&DecisionFilter::default(), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&decisions)?);
            } else if decisions.is_empty() {
                println!("(no permission decisions)");
            } else {
                for decision in decisions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        decision.id,
                        decision.time,
                        decision.session_id,
                        decision.agent,
                        decision.tool,
                        decision.outcome.as_str(),
                        crate::permission::decisions::scores_summary(&decision)
                    );
                }
            }
        }
    }
    Ok(EXIT_DONE)
}

#[derive(Args, Debug)]
pub struct HtmlConvertArgs {
    #[arg(long, value_enum)]
    pub format: crate::tools::fetch::ConvertFormat,
}

pub mod doctor;
mod embedded;
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
    /// Path of the models file for the embedded server.
    #[arg(long, conflicts_with = "url")]
    pub models: Option<PathBuf>,
    /// Connect to a running server instead of starting one. If it needs a bearer token, set
    /// WITHNAV_SERVER_TOKEN.
    #[arg(long, conflicts_with = "models")]
    pub url: Option<String>,
}

#[derive(Args, Debug)]
pub struct WebArgs {
    /// A workspace directory to offer in the browser; repeat for several. Defaults to the current
    /// directory.
    #[arg(long = "workspace", value_name = "PATH")]
    pub workspaces: Vec<PathBuf>,
    /// Path of the models file for the embedded server.
    #[arg(long)]
    pub models: Option<PathBuf>,
    /// Loopback port to listen on; 0 picks a free one.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// Print the address without opening the browser.
    #[arg(long)]
    pub no_open: bool,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Bind address; loopback-only unless `--allow-non-loopback` is also set.
    #[arg(long, default_value = "127.0.0.1:7310")]
    pub bind: String,

    /// Permit a non-loopback bind (still NO auth/TLS this slice — operator's
    /// reverse-proxy / private-network responsibility).
    #[arg(long)]
    pub allow_non_loopback: bool,

    #[arg(long)]
    pub models: Option<PathBuf>,
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
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .try_init();
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Commands::Doctor(args) => doctor::command(args).await,
        Commands::Permissions { command } => {
            tokio::task::spawn_blocking(move || permissions_command(command)).await?
        }
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

/// `run`: a one-shot IN-PROCESS CLIENT of the same core `serve` uses.
/// It starts the session manager (sequencer + runtime task, NO HTTP
/// server), subscribes to the session's event stream BEFORE posting the
/// prompt (so no initial `llm.delta` is lost — deltas are not in the replay
/// ring), renders each event through the `event_to_ui` adapter,
/// and derives the exit code from the terminal events — NOT from a direct
/// `run_agent` call (no shortcut). Order: create -> subscribe -> post ->
/// render-until-terminal -> close.
async fn run_command(args: RunArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let workspace = match args.workspace {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    let cfg_dir = config_dir()?;
    let rules_roots = RulesRoots::from_user_home();
    crate::session::preflight::check(&cfg_dir, args.models.as_deref(), &workspace, &rules_roots)
        .await?;
    let prompt = read_prompt(args.prompt)?;

    let state = crate::server::session::AppState::new(ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback"),
        models_path: args.models.clone(),
        config_dir: cfg_dir,
        allow_non_loopback: false,
        bearer_token: None,
        history_db: history_db_path()?,
        data_dir: crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
        rules_roots,
        web: None,
    })?;
    // A session that cannot start (no window, unknown alias, no key) ends the
    // run the way a failed turn does: the `error:` line, exit 1.
    let session_id = match state
        .create_session_with(
            &workspace,
            args.model.as_deref(),
            false,
            crate::session::VariantChoice::from_word(args.variant.as_deref()),
            args.agent.as_deref(),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!("\nerror: {e}");
            return Ok(EXIT_ERROR);
        }
    };
    let handle = state
        .handle(session_id)
        .ok_or_else(|| anyhow::anyhow!("session vanished immediately after creation"))?;

    // Ctrl-C cancels the SESSION token the runtime watches, so the in-flight
    // turn is interrupted and the runtime emits turn.end{interrupted} — the
    // render loop then exits 130. (Cancelling a standalone token would not
    // reach the runtime.)
    install_sigint_handler(handle.session_cancel.clone());

    // Subscribe BEFORE posting: deltas are live-only, so a late
    // subscriber would drop the initial streamed text.
    let mut sub = state
        .subscribe(session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe failed: {e}"))?;

    // A leading `/name` runs a session command and prints its result; `//text`
    // sends the literal message `/text`.
    let prompt = match crate::session::route_line(&prompt) {
        crate::session::Routed::Command { name, arguments } => {
            let exit = match state.run_command(session_id, &name, &arguments).await {
                Ok(outcome) if outcome.ok => {
                    println!("{}", outcome.render_text());
                    EXIT_DONE
                }
                Ok(outcome) => {
                    eprintln!("error: {}", outcome.render_text());
                    EXIT_ERROR
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    EXIT_ERROR
                }
            };
            let _ = state.close_session(session_id).await;
            return Ok(exit);
        }
        crate::session::Routed::Message(text) => text,
    };

    // Bypass is on before the first prompt; no fallback if it cannot be.
    if args.yes {
        if let Err(e) = state
            .set_bypass(
                session_id,
                true,
                crate::bus::runtime::bypass::BypassOrigin::Cli,
            )
            .await
        {
            eprintln!("\nerror: cannot enable bypass: {e}");
            let _ = state.close_session(session_id).await;
            return Ok(EXIT_ERROR);
        }
    }

    post_prompt(&state, session_id, &prompt).await?;

    let mut renderer = Renderer::new(args.show_thinking);
    let exit = render_until_terminal(&mut sub, &mut renderer).await;

    let _ = state.close_session(session_id).await;
    Ok(exit)
}

/// Post the user prompt as a `next-turn` command through the in-process
/// inbox, sequencing `message.received` before the command is consumed.
async fn post_prompt(
    state: &crate::server::session::AppState,
    session_id: ulid::Ulid,
    prompt: &str,
) -> anyhow::Result<()> {
    use crate::bus::command::{Command, ContentBlock, DeliveryMode};
    use crate::bus::event::ROOT_ACTOR_ID;

    let command = Command {
        command_id: ulid::Ulid::generate(),
        session_id,
        source_actor_id: "user:cli".to_string(),
        target_actor_id: ROOT_ACTOR_ID.to_string(),
        mode: DeliveryMode::NextTurn,
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
        created_at: std::time::SystemTime::now(),
    };
    state
        .send(command)
        .await
        .map_err(|e| anyhow::anyhow!("post failed: {e}"))?;
    Ok(())
}

/// Render events through the `event_to_ui` adapter until a terminal
/// `turn.end`, reproducing the Phase-1 `finish` behavior (stdout via the
/// UiEvents, plus the stderr line + exit code for interrupted/error) from the
/// event stream instead of the raw `RunOutcome`.
///
/// A `permission.requested` ends the run with [`EXIT_PERMISSION_REQUIRED`] and
/// a `permission_required: <tool> <scope>: <reason>` line on stderr. Nothing
/// is ever approved here: with `--yes` the session's bypass already answered
/// the ordinary requests, so one that arrives is one the Captain must decide.
async fn render_until_terminal(
    sub: &mut crate::session::Subscription,
    renderer: &mut Renderer,
) -> i32 {
    if let crate::bus::ResumeOutcome::Replay(events) = &sub.replay {
        for env in events {
            if let Some(ui) = event_to_ui(env) {
                renderer.render(ui);
            }
        }
    }
    let Some(live) = sub.live.as_mut() else {
        return EXIT_ERROR;
    };
    // Ctrl-C cancels the session token, so cancellation arrives here as a real
    // turn.end{interrupted} event — just read the live stream to the terminal.
    while let Some(env) = live.recv().await {
        if let Some(ui) = event_to_ui(&env) {
            renderer.render(ui);
        }
        if env.kind == EventKind::PermissionRequested.as_str() {
            let Ok(request) = serde_json::from_value::<crate::bus::event::PermissionRequested>(
                env.payload.clone(),
            ) else {
                return EXIT_ERROR;
            };
            eprintln!(
                "\npermission_required: {} {}: {}",
                request.tool, request.scope, request.reason
            );
            return EXIT_PERMISSION_REQUIRED;
        }
        if env.kind == EventKind::TurnEnd.as_str() {
            return exit_code_for(&env);
        }
    }
    EXIT_ERROR
}

/// Derive the process exit code from a `turn.end` envelope AND print the same
/// stderr line Phase-1 `finish` did (Interrupted/Error). The exact stderr text
/// is produced by [`terminal_stderr_line`] so a parity test can assert the
/// REAL string the production path prints. Done->0, Yielded->2,
/// Interrupted->130 (+stderr), Error->1 (+stderr with the bounded summary).
pub fn exit_code_for(env: &EventEnvelope) -> i32 {
    let Ok(payload) = serde_json::from_value::<TurnEnd>(env.payload.clone()) else {
        return EXIT_ERROR;
    };
    if let Some(line) = terminal_stderr_line(&payload) {
        eprint!("{line}");
    }
    match payload.outcome {
        TurnEndOutcome::Done => EXIT_DONE,
        TurnEndOutcome::Yielded => EXIT_YIELDED,
        TurnEndOutcome::Interrupted => EXIT_INTERRUPTED,
        TurnEndOutcome::Error => EXIT_ERROR,
    }
}

/// The stderr line the terminal outcome prints, matching Phase-1 `finish`:
/// `\ninterrupted` for Interrupted, `\nerror: {message}` for Error, and
/// nothing (stdout-only via `event_to_ui`) for Done/Yielded.
pub fn terminal_stderr_line(payload: &TurnEnd) -> Option<String> {
    match payload.outcome {
        TurnEndOutcome::Interrupted => Some("\ninterrupted".to_string()),
        TurnEndOutcome::Error => {
            let msg = payload
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_default();
            Some(format!("\nerror: {msg}"))
        }
        TurnEndOutcome::Done | TurnEndOutcome::Yielded => None,
    }
}

/// The shared conversation database, in WithNav's data dir.
fn history_db_path() -> anyhow::Result<std::path::PathBuf> {
    crate::paths::history_db().ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))
}

async fn serve_command(args: ServeArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let bind: std::net::SocketAddr = args
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {:?}: {e}", args.bind))?;
    let cfg_dir = config_dir()?;
    let rules_roots = RulesRoots::from_user_home();
    crate::session::preflight::check(
        &cfg_dir,
        args.models.as_deref(),
        &std::env::current_dir()?,
        &rules_roots,
    )
    .await?;
    let config = ServerConfig {
        bind,
        models_path: args.models,
        config_dir: cfg_dir,
        allow_non_loopback: args.allow_non_loopback,
        bearer_token: None,
        history_db: history_db_path()?,
        data_dir: crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
        rules_roots,
        web: None,
    };
    crate::server::routes::serve(
        config,
        std::sync::Arc::new(crate::navigator::NavigatorHooks::new()),
    )
    .await?;
    Ok(EXIT_DONE)
}

/// Render-behind-a-subscriber adapter : maps a bus event
/// onto the SAME UiEvent the Phase-1 `on_delta` closure + `finish` produced,
/// so `render.rs` and CLI stdout do not change.
/// Returns `None` for events with no CLI rendering (session/turn lifecycle,
/// llm.response, tool calls, agent.state, task.done, message.received) — the
/// Phase-1 path rendered none of those either.
pub fn event_to_ui(envelope: &EventEnvelope) -> Option<UiEvent> {
    let kind = EventKind::from_wire(&envelope.kind)?;
    match kind {
        EventKind::CommandResult if envelope.payload["name"] == "watchdog" => {
            Some(UiEvent::AssistantText(format!(
                "\n{}\n",
                envelope.payload["data"]["message"].as_str()?
            )))
        }
        EventKind::LlmDelta => {
            let payload: LlmDelta = serde_json::from_value(envelope.payload.clone()).ok()?;
            match payload.delta {
                crate::provider::StreamDelta::Text { text } => Some(UiEvent::AssistantText(text)),
                crate::provider::StreamDelta::Thinking { text } => Some(UiEvent::Thinking(text)),
                crate::provider::StreamDelta::ToolCallStarted { name, .. } => {
                    Some(UiEvent::ToolStarted { name })
                }
                crate::provider::StreamDelta::ToolCallArgs { .. } => None,
            }
        }
        EventKind::AgentYield => {
            let payload: AgentYield = serde_json::from_value(envelope.payload.clone()).ok()?;
            Some(UiEvent::Yielded {
                question: payload.question,
                kind: payload.kind,
            })
        }
        EventKind::TurnEnd => {
            let payload: TurnEnd = serde_json::from_value(envelope.payload.clone()).ok()?;
            match payload.outcome {
                TurnEndOutcome::Done => Some(UiEvent::Done),
                // `finish` renders the same Yielded UiEvent for both (the
                // budget case pins question="step budget exhausted",
                // kind="budget"), and prints "interrupted"/"error" to stderr
                // without a UiEvent — so neither maps to a UiEvent here.
                TurnEndOutcome::Yielded | TurnEndOutcome::Interrupted | TurnEndOutcome::Error => {
                    None
                }
            }
        }
        EventKind::SessionStart
        | EventKind::SessionEnd
        | EventKind::MessageReceived
        | EventKind::TurnStart
        | EventKind::LlmResponse
        | EventKind::ToolPreCall
        | EventKind::ToolPostCall
        | EventKind::AgentState
        | EventKind::TaskDone
        | EventKind::CommandResult
        | EventKind::SessionAgentChanged
        | EventKind::PermissionRequested
        | EventKind::PermissionResolved
        | EventKind::ModelChanged
        | EventKind::SubagentSpawned
        | EventKind::SubagentDone
        | EventKind::SubagentWatchdog
        | EventKind::ContextCompacted => None,
        EventKind::ContextCompacting => Some(UiEvent::AssistantText(format!(
            "Compacting… {}/{}",
            envelope.payload["pass"], envelope.payload["total"]
        ))),
        EventKind::ContextUpdated | EventKind::SessionBypass => None,
    }
}

fn read_prompt(prompt: Option<String>) -> anyhow::Result<String> {
    match prompt {
        Some(p) if p != "-" => Ok(p),
        _ => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf.trim().to_string())
        }
    }
}

fn install_sigint_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });
}

#[cfg(test)]
mod tests;
