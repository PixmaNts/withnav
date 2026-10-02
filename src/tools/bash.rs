use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{bash_grace_ms, bound_output, Tool, ToolContext, ToolResult, ToolStatus};
use crate::tools::{DEFAULT_BASH_TIMEOUT_MS, DEFAULT_MAX_OUTPUT_BYTES};

const READER_JOIN_DEADLINE_MS: u64 = 500;

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Run a shell command in the workspace. Supports timeout and cancellation; kills the whole process group.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "timeout in milliseconds"}
                },
                "required": ["command"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Denied {
                    code: "invalid_args".into(),
                    message: "missing string argument: command".into(),
                })?
                .to_string();
            let timeout_ms = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_BASH_TIMEOUT_MS);
            run_bash(&command, cx, cancel, timeout_ms, bash_grace_ms()).await
        })
    }
}

async fn run_bash(
    command: &str,
    cx: &ToolContext,
    cancel: CancellationToken,
    timeout_ms: u64,
    grace_ms: u64,
) -> Result<ToolResult, ToolError> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(&cx.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;

    let pid = child.id().ok_or_else(|| ToolError::Denied {
        code: "spawn_failed".into(),
        message: "child process exited before a pid was available".into(),
    })? as i32;
    let pgid = Pid::from_raw(pid);

    let stdout = child.stdout.take().ok_or_else(|| ToolError::Denied {
        code: "spawn_failed".into(),
        message: "child stdout pipe missing".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Denied {
        code: "spawn_failed".into(),
        message: "child stderr pipe missing".into(),
    })?;
    let mut out_reader = spawn_reader(stdout);
    let mut err_reader = spawn_reader(stderr);

    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    enum Ending {
        Exited(std::process::ExitStatus),
        Canceled,
        TimedOut,
    }

    let ending = tokio::select! {
        status = child.wait() => Ending::Exited(status?),
        _ = cancel.cancelled() => Ending::Canceled,
        _ = &mut timeout => Ending::TimedOut,
    };

    let exit_status = match ending {
        Ending::Exited(status) => Some(status),
        Ending::Canceled | Ending::TimedOut => {
            terminate_group(pgid, grace_ms).await;
            let _ = child.wait().await;
            None
        }
    };

    let stdout_text = join_reader(&mut out_reader).await;
    let stderr_text = join_reader(&mut err_reader).await;

    match ending {
        Ending::Canceled => return Ok(ToolResult::interrupted()),
        Ending::TimedOut => return Err(ToolError::Timeout),
        Ending::Exited(_) => {}
    }

    let mut combined = String::new();
    combined.push_str(&stdout_text);
    if !stderr_text.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr_text);
    }
    let (output, truncated) = bound_output(&combined, DEFAULT_MAX_OUTPUT_BYTES);
    let code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
    let status = if code == 0 {
        ToolStatus::Ok
    } else {
        ToolStatus::Error
    };
    let mut result = ToolResult {
        status,
        output,
        truncated: None,
        error: None,
        metadata: json!({"exit_code": code}),
    };
    if let Some(orig) = truncated {
        result = result.with_truncation(orig);
    }
    Ok(result)
}

struct Reader {
    handle: tokio::task::JoinHandle<String>,
}

fn spawn_reader<R>(reader: R) -> Reader
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut r = BufReader::new(reader);
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < DEFAULT_MAX_OUTPUT_BYTES * 2 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    Reader { handle }
}

async fn join_reader(reader: &mut Reader) -> String {
    match tokio::time::timeout(
        Duration::from_millis(READER_JOIN_DEADLINE_MS),
        &mut reader.handle,
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => String::new(),
        Err(_) => {
            reader.handle.abort();
            String::new()
        }
    }
}

async fn terminate_group(pgid: Pid, grace_ms: u64) {
    if matches!(killpg(pgid, Signal::SIGTERM), Err(nix::errno::Errno::ESRCH)) {
        return;
    }
    tokio::time::sleep(Duration::from_millis(grace_ms)).await;
    let _ = killpg(pgid, Signal::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[tokio::test]
    async fn echo_captures_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let res = run_bash(
            "echo hi",
            &ctx(tmp.path()),
            CancellationToken::new(),
            5000,
            200,
        )
        .await
        .unwrap();
        assert_eq!(res.status, ToolStatus::Ok);
        assert!(res.output.contains("hi"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_status() {
        let tmp = tempfile::tempdir().unwrap();
        let res = run_bash(
            "exit 3",
            &ctx(tmp.path()),
            CancellationToken::new(),
            5000,
            200,
        )
        .await
        .unwrap();
        assert_eq!(res.status, ToolStatus::Error);
        assert_eq!(res.metadata["exit_code"], 3);
    }

    #[tokio::test]
    async fn timeout_returns_timeout_within_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let err = run_bash(
            "sleep 30",
            &ctx(tmp.path()),
            CancellationToken::new(),
            1000,
            200,
        )
        .await
        .unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, ToolError::Timeout));
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn cancel_kills_process_group_including_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("child.pid");
        let cmd = format!("sleep 30 & echo $! > {}; wait", pidfile.to_str().unwrap());
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn({
            let cx = ctx(tmp.path());
            async move { run_bash(&cmd, &cx, cancel2, 30_000, 200).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let descendant_pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        cancel.cancel();
        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let alive = matches!(
            nix::sys::signal::kill(Pid::from_raw(descendant_pid), None),
            Ok(())
        );
        assert!(!alive, "descendant {descendant_pid} should be dead (ESRCH)");
    }

    #[tokio::test]
    async fn kill_path_reaches_child_that_ignores_term() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let err = run_bash(
            "trap '' TERM; sleep 30",
            &ctx(tmp.path()),
            CancellationToken::new(),
            700,
            300,
        )
        .await
        .unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, ToolError::Timeout));
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn setsid_escaped_descendant_does_not_hang_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = "2718281";
        let cmd = format!("setsid sleep {marker} >/dev/null 2>&1 & echo started; sleep {marker}");
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn({
            let cx = ctx(tmp.path());
            async move { run_bash(&cmd, &cx, cancel2, 30_000, 200).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let start = std::time::Instant::now();
        cancel.cancel();
        let res = handle.await.unwrap().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(res.status, ToolStatus::Interrupted);
        assert!(
            elapsed < Duration::from_secs(2),
            "tool must return within deadline+grace even with a setsid escapee; took {elapsed:?}"
        );
        let cleanup = format!("pkill -f 'sleep {marker}' || true");
        let _ = run_bash(
            &cleanup,
            &ctx(tmp.path()),
            CancellationToken::new(),
            2000,
            200,
        )
        .await;
    }
}
