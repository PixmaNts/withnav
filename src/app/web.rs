//! CLI adapter for the browser client: starts the private session server with the embedded web
//! app, prints the address (the token travels in the URL fragment, which a browser never sends to
//! a server), opens it, and runs until Ctrl-C.

use std::path::PathBuf;

use anyhow::Context as _;

use super::embedded::{resolve_workspace, EmbeddedServer};
use super::{WebArgs, EXIT_DONE};
use crate::server::WebConfig;

pub async fn run(args: WebArgs) -> anyhow::Result<i32> {
    let requested = if args.workspaces.is_empty() {
        vec![std::env::current_dir().context("cannot read the current directory")?]
    } else {
        args.workspaces
    };
    let mut workspaces: Vec<PathBuf> = Vec::new();
    for path in &requested {
        let resolved = resolve_workspace(path)?;
        if !workspaces.contains(&resolved) {
            workspaces.push(resolved);
        }
    }
    let server = EmbeddedServer::start(
        args.models.as_deref(),
        &workspaces,
        Some(WebConfig {
            launch_workspaces: workspaces.clone(),
        }),
        args.port,
    )
    .await?;
    let address = format!("{}/#token={}", server.url, server.token);
    println!("Navai WebUI: {address}");
    if !args.no_open {
        if let Err(error) = open_browser(&address) {
            println!("Could not open a browser ({error}); open the address above yourself.");
        }
    }
    let waited = tokio::signal::ctrl_c().await;
    server.stop().await;
    waited.context("cannot listen for Ctrl-C")?;
    Ok(EXIT_DONE)
}

/// Hands `url` to the desktop's opener; it is not waited for.
fn open_browser(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}
