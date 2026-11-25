//! The editor server and offline replay entry points for `nav lsp`.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct LspArgs {
    /// Explicit path to models.json (default: workspace, then config dir).
    #[arg(long)]
    pub models: Option<PathBuf>,
    /// Accepted and ignored: vscode-languageclient appends this flag, and
    /// stdio is the editor server's only transport.
    #[arg(long, hide = true)]
    pub stdio: bool,
    #[command(subcommand)]
    pub command: Option<LspCommand>,
}

#[derive(Subcommand, Debug)]
pub enum LspCommand {
    /// Replays built-in editor flows offline against captured decisions.
    Replay {
        /// Accepted for compatibility; flows are the only replay mode.
        #[arg(long)]
        flows: bool,
        /// Decision log to replay (default: the navigator log).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Machine-readable report instead of a summary.
        #[arg(long)]
        json: bool,
    },
}

pub fn init_tracing_stderr() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // stdout carries LSP frames; tracing must never write into that channel.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(super::logging::mcp_safe_filter())
        .with(
            fmt::layer()
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_writer(std::io::stderr),
        )
        .try_init();
}

pub async fn run(args: LspArgs) -> anyhow::Result<i32> {
    if let Some(LspCommand::Replay { log, json, .. }) = args.command {
        let path = log
            .or_else(crate::paths::decision_log)
            .ok_or_else(|| anyhow::anyhow!("pass --log"))?;
        let report = crate::navigator::replay::flows::report(&std::fs::read_to_string(path)?);
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{} compared, {} changed, {} not comparable",
                report["compared"],
                report["changed"].as_array().map_or(0, Vec::len),
                report["not_comparable"].as_array().map_or(0, Vec::len)
            );
        }
        Ok(super::EXIT_DONE)
    } else {
        run_bridge(args).await
    }
}

async fn run_bridge(args: LspArgs) -> anyhow::Result<i32> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let bridge_cwd = std::env::current_dir()?;
    let mut stdin = tokio::io::stdin();
    let Some((init_frame, workspace)) =
        crate::navigator::bootstrap::initial_workspace(&mut stdin, &bridge_cwd).await?
    else {
        return Ok(super::EXIT_DONE);
    };

    let wanted = crate::app::connect::Wanted::from_env(args.models.as_deref())?;
    let owner = crate::app::connect::ensure_server(&wanted).await?;

    let port = owner.record.port;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to owner on 127.0.0.1:{port}: {e}"))?;

    let encoded_ws = crate::navigator::lsp::uri::percent_encode(workspace.to_str().unwrap_or(""));
    let req = format!(
        "GET /v1/lsp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: withnav-lsp\r\n\
         x-withnav-workspace: {encoded_ws}\r\n\
         x-withnav-cwd: {encoded_ws}\r\n\
         \r\n",
        owner.token
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("unexpected EOF waiting for owner upgrade response");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 65536 {
            anyhow::bail!("owner upgrade response headers exceeded 64 KiB");
        }
    }

    let headers_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = headers_str.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let is_101 = status_line.contains(" 101 ");

    if !is_101 {
        let content_length = lines.find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        let mut body = buf[header_end..].to_vec();
        if let Some(cl) = content_length {
            while body.len() < cl {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
        }
        let body_text = String::from_utf8_lossy(&body);
        let error_msg = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_text) {
            val.get("error")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .or_else(|| {
                    val.get("reason")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| body_text.trim().to_string())
        } else {
            body_text.trim().to_string()
        };
        eprintln!("{error_msg}");
        return Ok(1);
    }

    stream.write_all(&init_frame).await?;
    stream.flush().await?;

    let leftover = buf[header_end..].to_vec();
    let mut stdout = tokio::io::stdout();
    if !leftover.is_empty() {
        stdout.write_all(&leftover).await?;
        stdout.flush().await?;
    }

    let (mut stream_read, mut stream_write) = stream.into_split();

    let copy_server_to_stdout = async {
        let mut buf = [0u8; 16384];
        loop {
            match stream_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    let copy_stdin_to_server = async {
        let mut buf = [0u8; 16384];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => {
                    let _ = stream_write.shutdown().await;
                    futures::future::pending::<()>().await;
                }
                Ok(n) => {
                    if stream_write.write_all(&buf[..n]).await.is_err()
                        || stream_write.flush().await.is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    let _ = stream_write.shutdown().await;
                    futures::future::pending::<()>().await;
                }
            }
        }
    };

    tokio::select! {
        _ = copy_server_to_stdout => {
            std::process::exit(0);
        }
        _ = copy_stdin_to_server => {}
    }

    Ok(super::EXIT_DONE)
}
