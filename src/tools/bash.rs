#[cfg(unix)]
#[path = "bash_unix.rs"]
mod unix;
#[cfg(unix)]
pub(crate) use unix::script_matches;
#[cfg(unix)]
pub use unix::{init_shell, trusted_shell, BashTool};

#[cfg(windows)]
mod windows {
    use crate::error::ToolError;
    use crate::tools::{Tool, ToolContext, ToolResult};
    use serde_json::{json, Value};
    use std::{future::Future, pin::Pin};
    use tokio_util::sync::CancellationToken;
    // Trusted Git Bash discovery and Job Object containment must precede execution.
    pub fn init_shell() {}
    pub fn trusted_shell() -> Option<&'static std::path::Path> {
        None
    }
    pub(crate) fn script_matches(_: &Value) -> bool {
        false
    }
    pub struct BashTool;
    impl Tool for BashTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn schema(&self) -> Value {
            json!({"description":"Run a shell command in the workspace. Supports timeout and cancellation; kills the whole process group.","parameters":{"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","description":"timeout in milliseconds"}},"required":["command"]}})
        }
        fn call<'a>(
            &'a self,
            _: Value,
            _: CancellationToken,
            _: &'a ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
            Box::pin(async {
                Err(ToolError::Denied {
                    code: "unsupported_shell".into(),
                    message: "shell execution is not yet supported on Windows".into(),
                })
            })
        }
    }
    #[test]
    fn shell_and_script_identity_are_unavailable() {
        assert!(trusted_shell().is_none());
        assert!(!script_matches(&Value::Null));
    }
    #[tokio::test]
    async fn shell_execution_fails_closed() {
        let workspace = std::env::temp_dir();
        let cx = ToolContext {
            binding: None,
            workspace,
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
        };
        let error = BashTool
            .call(
                json!({"command": "echo unsafe"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied { message, .. }
            if message == "shell execution is not yet supported on Windows"));
    }
}
#[cfg(windows)]
pub(crate) use windows::script_matches;
#[cfg(windows)]
pub use windows::{init_shell, trusted_shell, BashTool};
