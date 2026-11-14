//! CLI adapter for the browser client: connects to the background session server, mints a pairing
//! code for the requested launch workspaces, prints and opens the WebUI address with the single-use
//! pairing code, and exits immediately.

use std::path::PathBuf;

use anyhow::Context as _;

use super::connect;
use super::embedded::resolve_workspace;
use super::{EXIT_DONE, WebArgs};

pub async fn run(args: WebArgs) -> anyhow::Result<i32> {
    if args.port != 0 {
        anyhow::bail!("the shared WithNav server picks its port; use `nav serve --port` to set it");
    }
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
    let wanted = connect::Wanted::from_env(args.models.as_deref())?;
    let owner = connect::ensure_server(&wanted).await?;
    let pair_url = format!("{}/v1/pair", owner.url);
    let client = reqwest::Client::new();
    let response = client
        .post(&pair_url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", owner.token),
        )
        .json(&serde_json::json!({ "workspaces": workspaces }))
        .send()
        .await
        .context("cannot contact WithNav server to mint pairing code")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("failed to mint pairing code (HTTP {status}): {body}");
    }

    let pair_resp: serde_json::Value = response
        .json()
        .await
        .context("cannot parse pairing response")?;
    let code = pair_resp
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing pairing code in response"))?;

    let address = format!("{}/#pair={code}", owner.url);
    println!("WithNav WebUI: {address}");
    if !args.no_open
        && let Err(error) = open_browser(&address)
    {
        println!("Could not open a browser ({error}); open the address above yourself.");
    }
    Ok(EXIT_DONE)
}

/// Hands `url` to the desktop's opener; it is not waited for.
#[cfg(not(windows))]
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

#[cfg(windows)]
fn open_browser(url: &str) -> std::io::Result<()> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    // ShellExecute treats the URL as a target, never as shell command text.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize <= 32 {
        Err(std::io::Error::other(format!(
            "ShellExecuteW failed ({})",
            result as usize
        )))
    } else {
        Ok(())
    }
}
