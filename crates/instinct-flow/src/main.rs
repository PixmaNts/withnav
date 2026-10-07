use clap::{Parser, Subcommand};
use instinct_flow::*;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
#[derive(Parser)]
#[command(about = "Validate, sample and replay host-independent decision graphs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Validate {
        dir: PathBuf,
        #[arg(long)]
        catalogue: PathBuf,
    },
    Run {
        flow: PathBuf,
        #[arg(long)]
        payload: PathBuf,
        #[arg(long)]
        catalogue: PathBuf,
        #[arg(long)]
        endpoint: url::Url,
        #[arg(long)]
        model: String,
    },
    Replay {
        flow: PathBuf,
        #[arg(long)]
        runs: PathBuf,
        #[arg(long)]
        catalogue: PathBuf,
    },
}
struct AbsentInputs;
impl InputSource for AbsentInputs {
    fn collect<'a>(
        &'a self,
        names: &'a [String],
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<BTreeMap<String, InputValue>, InputError>> {
        Box::pin(async move {
            Ok(names
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        InputValue {
                            value: None,
                            cut: false,
                            redacted: false,
                        },
                    )
                })
                .collect())
        })
    }
}
fn read(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|_| format!("cannot read {}", path.display()))?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 33554432 {
        return Err("input must be a bounded regular file, not a symlink".into());
    }
    std::fs::read_to_string(path).map_err(|_| format!("cannot read UTF-8 file {}", path.display()))
}
fn load(path: &Path, catalogue: &Catalogue) -> Result<CompiledDefinition, String> {
    let definition = Definition::parse(&read(path)?).map_err(diagnostics)?;
    if path.file_stem().and_then(|s| s.to_str()) != Some(&definition.flow.name) {
        return Err("flow name must match filename stem".into());
    }
    let mut templates = BTreeMap::new();
    for node in &definition.node {
        if let Some(name) = node.kind.template() {
            if !validation::template_name(name) {
                return Err("template must be relative sibling".into());
            }
            templates.insert(
                name.into(),
                read(&path.parent().unwrap_or(Path::new(".")).join(name))?,
            );
        }
    }
    definition
        .validate(catalogue, &Capabilities { images: false }, &templates)
        .map_err(diagnostics)
}
fn diagnostics(errors: Vec<Diagnostic>) -> String {
    serde_json::to_string_pretty(&errors).unwrap_or_else(|_| "validation failed".into())
}
#[tokio::main]
async fn main() {
    if let Err(error) = execute(Cli::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn execute(cli: Cli) -> Result<(), String> {
    let catalogue_path = match &cli.command {
        Command::Validate { catalogue, .. }
        | Command::Run { catalogue, .. }
        | Command::Replay { catalogue, .. } => catalogue,
    };
    let catalogue: Catalogue =
        serde_json::from_str(&read(catalogue_path)?).map_err(|_| "invalid catalogue JSON")?;
    catalogue.validate().map_err(diagnostics)?;
    match cli.command {
        Command::Validate { dir, .. } => {
            let mut files = std::fs::read_dir(dir)
                .map_err(|_| "cannot read flow directory")?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot read directory entry")?;
            files.sort();
            let mut errors = Vec::new();
            for path in files {
                if path.extension().is_some_and(|s| s == "toml") {
                    match load(&path, &catalogue) {
                        Ok(_) => println!("{}: valid", path.display()),
                        Err(error) => errors.push(format!("{}: {error}", path.display())),
                    }
                }
            }
            if !errors.is_empty() {
                return Err(errors.join("\n"));
            }
        }
        Command::Run {
            flow,
            payload,
            endpoint,
            model,
            ..
        } => {
            let compiled = load(&flow, &catalogue)?;
            let payload =
                serde_json::from_str(&read(&payload)?).map_err(|_| "invalid event JSON")?;
            let event = Event {
                trigger: compiled.definition().flow.on.clone(),
                payload,
            };
            let key =
                std::env::var("INSTINCT_API_KEY").map_err(|_| "INSTINCT_API_KEY is required")?;
            let client = SystemOneClient::new(
                endpoint,
                model,
                key,
                Duration::from_secs(2),
                BTreeMap::new(),
            )
            .map_err(|e| e.to_string())?;
            let outcome = run(
                &compiled,
                &event,
                &AbsentInputs,
                &client,
                RunOptions {
                    deadline: Instant::now() + Duration::from_secs(5),
                    run_id: ulid::Ulid::new().to_string(),
                },
                &CancellationToken::new(),
            )
            .await;
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome.trace)
                    .map_err(|_| "cannot serialize record")?
            );
        }
        Command::Replay { flow, runs, .. } => {
            let compiled = load(&flow, &catalogue)?;
            for line in read(&runs)?.lines().filter(|line| !line.trim().is_empty()) {
                let record: RunRecord =
                    serde_json::from_str(line).map_err(|_| "invalid run JSONL record")?;
                println!(
                    "{}",
                    serde_json::to_string(&replay(&compiled, &record, &record.identity))
                        .map_err(|_| "cannot serialize replay")?
                );
            }
        }
    }
    Ok(())
}
