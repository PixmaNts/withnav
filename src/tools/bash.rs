use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{killpg, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{bash_grace_ms, bound_output, Tool, ToolContext, ToolResult, ToolStatus};
use crate::tools::{DEFAULT_BASH_TIMEOUT_MS, DEFAULT_MAX_OUTPUT_BYTES};

const READER_JOIN_DEADLINE_MS: u64 = 500;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix::bash_command;
#[cfg(all(unix, test))]
use unix::trusted_launcher;
#[cfg(unix)]
pub use unix::{init_shell, trusted_shell};
#[cfg(windows)]
use windows::bash_command;
#[cfg(windows)]
pub use windows::{init_shell, trusted_shell};

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
    let mut launcher = bash_command()?;
    #[cfg(unix)]
    launcher.arg(command);
    #[cfg(windows)]
    windows::set_command(&mut launcher, command);
    launcher
        .current_dir(&cx.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    launcher.process_group(0);
    #[cfg(unix)]
    let mut child = launcher.spawn()?;
    #[cfg(windows)]
    let (mut child, job) = super::process_job::Job::spawn(&mut launcher, None)?;

    #[cfg(unix)]
    let pid = child.id().ok_or_else(|| ToolError::Denied {
        code: "spawn_failed".into(),
        message: "child process exited before a pid was available".into(),
    })?;
    #[cfg(unix)]
    let pgid = positive_pgid(pid).ok_or_else(|| ToolError::Denied {
        code: "spawn_failed".into(),
        message: "child process id is not a positive process group id".into(),
    })?;

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
            #[cfg(unix)]
            terminate_group(pgid, grace_ms).await;
            #[cfg(windows)]
            {
                let _ = grace_ms;
                job.terminate();
            }
            let _ = child.wait().await;
            None
        }
    };

    let stdout_text = join_reader(&mut out_reader).await;
    let stderr_text = join_reader(&mut err_reader).await;

    let mut combined = String::new();
    combined.push_str(&stdout_text);
    if !stderr_text.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr_text);
    }
    let (output, truncated) = bound_output(&combined, DEFAULT_MAX_OUTPUT_BYTES);
    match ending {
        Ending::Canceled => return Ok(ToolResult::interrupted_with(output)),
        Ending::TimedOut => return Err(ToolError::Timeout),
        Ending::Exited(_) => {}
    }
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
        let mut chunk = [0u8; 64 * 1024];
        loop {
            // Drain excess output in larger reads without slowing the command or
            // closing its pipe (which could deliver SIGPIPE).
            let read_size = if buf.len() < DEFAULT_MAX_OUTPUT_BYTES * 2 {
                8192
            } else {
                chunk.len()
            };
            match r.read(&mut chunk[..read_size]).await {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < DEFAULT_MAX_OUTPUT_BYTES * 2 {
                        let remaining = DEFAULT_MAX_OUTPUT_BYTES * 2 - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(remaining)]);
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

#[cfg(unix)]
fn positive_pgid(pid: u32) -> Option<Pid> {
    i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .map(Pid::from_raw)
}

#[cfg(unix)]
async fn terminate_group(pgid: Pid, grace_ms: u64) {
    if pgid.as_raw() <= 0 {
        return;
    }
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
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn impostor_bash_first_in_path_is_never_executed() {
        const CHILD: &str = "WITHNAV_TEST_IMPOSTOR_BASH";
        if let Some(root) = std::env::var_os(CHILD) {
            let res = run_bash(
                "echo trusted",
                &ctx(Path::new(&root)),
                CancellationToken::new(),
                5000,
                200,
            )
            .await
            .unwrap();
            assert_eq!(res.status, ToolStatus::Ok);
            assert_eq!(res.output.trim(), "trusted");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = tmp.path().join("executed");
        let impostor = bin.join("bash");
        std::fs::write(
            &impostor,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&impostor, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::bash::tests::impostor_bash_first_in_path_is_never_executed",
                "--nocapture",
            ])
            .env(CHILD, tmp.path())
            .env("PATH", format!("{}:/usr/bin", bin.display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn bash_env_and_exported_functions_are_not_sourced() {
        const CHILD: &str = "WITHNAV_TEST_BASH_ENV";
        if let Some(root) = std::env::var_os(CHILD) {
            let res = run_bash(
                "echo trusted",
                &ctx(Path::new(&root)),
                CancellationToken::new(),
                5000,
                200,
            )
            .await
            .unwrap();
            assert_eq!(res.status, ToolStatus::Ok);
            assert_eq!(res.output.trim(), "trusted");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("executed");
        let script = tmp.path().join("startup.sh");
        std::fs::write(&script, format!("touch '{}'\n", marker.display())).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::bash::tests::bash_env_and_exported_functions_are_not_sourced",
                "--nocapture",
            ])
            .env(CHILD, tmp.path())
            .env("BASH_ENV", &script)
            .env("ENV", &script)
            .env(
                "BASH_FUNC_echo%%",
                format!("() {{ touch '{}'; }}", marker.display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!marker.exists());
    }

    #[test]
    #[cfg(unix)]
    fn process_group_id_rejects_zero_and_overflow() {
        assert!(positive_pgid(0).is_none());
        assert!(positive_pgid(i32::MAX as u32 + 1).is_none());
        assert!(positive_pgid(u32::MAX).is_none());
        assert_eq!(positive_pgid(1).unwrap().as_raw(), 1);
        assert_eq!(positive_pgid(i32::MAX as u32).unwrap().as_raw(), i32::MAX);
    }

    #[tokio::test]
    async fn output_reader_caps_retained_bytes() {
        let input = vec![b'x'; DEFAULT_MAX_OUTPUT_BYTES * 2 + 8192];
        let mut reader = spawn_reader(std::io::Cursor::new(input));
        assert_eq!(
            join_reader(&mut reader).await.len(),
            DEFAULT_MAX_OUTPUT_BYTES * 2
        );
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
    #[cfg(unix)]
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
        // Under a loaded test run the shell can take well over a few hundred
        // milliseconds to start and write the pid: wait for the file instead
        // of sleeping a fixed time, bounded so a real failure still fails.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let descendant_pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never wrote the descendant's pid"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        cancel.cancel();
        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let alive = || {
            matches!(
                nix::sys::signal::kill(Pid::from_raw(descendant_pid), None),
                Ok(())
            )
        };
        while alive() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !alive(),
            "descendant {descendant_pid} should be dead (ESRCH)"
        );
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
    async fn a_cancelled_command_keeps_the_output_it_produced() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("started");
        let cmd = format!("echo partial-line; touch {}; sleep 30", marker.display());
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let (cx, cancel) = (ctx(tmp.path()), cancel.clone());
            async move { run_bash(&cmd, &cx, cancel, 30_000, 200).await }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(std::time::Instant::now() < deadline, "never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
        assert!(!res.never_started());
        assert!(res.output.contains("partial-line"), "{:?}", res.output);
    }

    #[tokio::test]
    #[cfg(unix)]
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

    #[test]
    #[cfg(unix)]
    fn launcher_rejects_user_owned_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("bash");
        std::fs::write(&fake, "").unwrap();
        assert!(super::trusted_launcher(&fake).is_none());
    }
}
