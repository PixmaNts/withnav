use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::provider::openai::ToolDef;

pub mod bash;
pub mod control;
pub mod decide_subagent;
pub mod fetch;
pub mod glob;
mod grep;
pub mod native;
pub mod nofollow;
pub mod steer_subagent;
pub mod task;

/// Legacy reserved argument name. Every model ingress rejects it; approved
/// destinations travel only in the host's dispatch context.
pub const BOUND_ARG: &str = "navai_bound_destination";
pub const OUTSIDE_SEARCH: &str =
    "outside the project: use `read` on a specific file, or a `bash` command, which are reviewed";

/// Model arguments cannot supply host bindings or scratch identities.
pub fn reject_reserved_args(args: &Value) -> Result<(), ToolError> {
    if args
        .as_object()
        .is_some_and(|object| object.keys().any(|key| key.starts_with("navai_")))
    {
        return Err(ToolError::Denied {
            code: "invalid_args".into(),
            message: "reserved tool argument".into(),
        });
    }
    Ok(())
}

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_BASH_GRACE_MS: u64 = 5_000;

pub fn bash_grace_ms() -> u64 {
    std::env::var("NAVAI_BASH_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BASH_GRACE_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Ok,
    Denied,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolErrorObj {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truncated {
    pub original_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncated>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorObj>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Ok,
            output: output.into(),
            truncated: None,
            error: None,
            metadata: Value::Null,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        let message = message.into();
        let message = if message.trim().is_empty() {
            "tool failed without an explanation".to_string()
        } else {
            message
        };
        Self {
            status: ToolStatus::Error,
            output: message.clone(),
            truncated: None,
            error: Some(ToolErrorObj {
                code: code.into(),
                plugin_id: None,
                message,
            }),
            metadata: Value::Null,
        }
    }

    /// A call a driver refused: the model reads `message` back.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Denied,
            output: String::new(),
            truncated: None,
            error: Some(ToolErrorObj {
                code: "permission_denied".into(),
                plugin_id: None,
                message: message.into(),
            }),
            metadata: Value::Null,
        }
    }

    pub fn interrupted() -> Self {
        Self {
            status: ToolStatus::Interrupted,
            output: String::new(),
            truncated: None,
            error: None,
            metadata: Value::Null,
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_truncation(mut self, original_bytes: usize) -> Self {
        self.truncated = Some(Truncated { original_bytes });
        self
    }
}

#[derive(Clone)]
pub struct ToolContext {
    /// Host-only binding installed by the agent loop, never model arguments.
    pub binding: Option<DispatchBinding>,
    pub workspace: PathBuf,
    /// Where `write`, `edit` and `bash` may change files (and `read`/`grep`
    /// may look).
    pub writable_roots: Vec<PathBuf>,
    /// Extra places `read` and `grep` may look, never change: the skill
    /// roots.
    pub readable_roots: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct DispatchBinding {
    pub(crate) destination: PathBuf,
    pub(crate) scratch: Option<crate::agent::scratch::Identity>,
}

impl ToolContext {
    /// The roots `read` and `grep` accept: the writable roots, then the
    /// read-only ones.
    pub fn read_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.writable_roots.clone();
        roots.extend(self.readable_roots.iter().cloned());
        roots
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>>;
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Removes tools outside a session's frozen capability ceiling. Both
    /// schemas and dispatch use this registry, so a model cannot bypass a
    /// hidden tool by naming it directly.
    pub fn retain(&mut self, mut allowed: impl FnMut(&str) -> bool) {
        self.tools.retain(|name, _| allowed(name));
    }

    pub fn schemas(&self) -> Vec<ToolDef> {
        self.schemas_filtered(|_| true)
    }

    /// Projects schemas without changing the session's frozen registry.
    pub fn schemas_filtered(&self, mut allowed: impl FnMut(&str) -> bool) -> Vec<ToolDef> {
        self.tools
            .values()
            .filter(|tool| allowed(tool.name()))
            .map(|t| {
                let schema = t.schema();
                let description = schema
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let parameters = schema
                    .get("parameters")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                ToolDef::function(t.name(), description, parameters)
            })
            .collect()
    }

    pub async fn dispatch(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
        cx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        reject_reserved_args(&args)?;
        match self.get(name) {
            Some(tool) => tool.call(args, cancel, cx).await,
            None => Ok(ToolResult {
                status: ToolStatus::Error,
                output: String::new(),
                truncated: None,
                error: Some(ToolErrorObj {
                    code: "unknown_tool".to_string(),
                    plugin_id: None,
                    message: format!("no such tool: {name}"),
                }),
                metadata: Value::Null,
            }),
        }
    }
}

pub fn validate_path(roots: &[PathBuf], candidate: &Path) -> Result<PathBuf, ToolError> {
    let existing = nearest_existing_ancestor(candidate);
    let canon_existing = existing
        .canonicalize()
        .map_err(|_| ToolError::PathTraversal(candidate.to_path_buf()))?;

    let tail = candidate
        .strip_prefix(&existing)
        .map_err(|_| ToolError::PathTraversal(candidate.to_path_buf()))?;

    let mut resolved = canon_existing;
    for comp in tail.components() {
        match comp {
            Component::Normal(seg) => resolved.push(seg),
            Component::CurDir => {}
            _ => return Err(ToolError::PathTraversal(candidate.to_path_buf())),
        }
    }

    if !roots.iter().any(|root| {
        root.canonicalize()
            .map(|r| resolved.starts_with(&r))
            .unwrap_or(false)
    }) {
        return Err(ToolError::PathTraversal(candidate.to_path_buf()));
    }
    Ok(resolved)
}

fn nearest_existing_ancestor(candidate: &Path) -> PathBuf {
    let mut existing = candidate.to_path_buf();
    while !existing.exists() {
        if !existing.pop() {
            break;
        }
    }
    existing
}

pub fn bound_output(s: &str, max_bytes: usize) -> (String, Option<usize>) {
    let original = s.len();
    if original <= max_bytes {
        return (s.to_string(), None);
    }
    let marker = "\n...[output elided]...\n";
    let budget = max_bytes.saturating_sub(marker.len());
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;

    let head_end = floor_char_boundary(s, head_budget);
    let tail_start = ceil_char_boundary(s, original.saturating_sub(tail_budget));

    let mut out = String::with_capacity(max_bytes);
    out.push_str(&s[..head_end]);
    out.push_str(marker);
    out.push_str(&s[tail_start..]);
    (out, Some(original))
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyErrTool;
    impl Tool for DummyErrTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn schema(&self) -> Value {
            serde_json::json!({"description":"d","parameters":{"type":"object"}})
        }
        fn call<'a>(
            &'a self,
            _args: Value,
            _cancel: CancellationToken,
            _cx: &'a ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
            Box::pin(async { Ok(ToolResult::error("boom", "it failed")) })
        }
    }

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[test]
    fn validate_path_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let evil = root.join("../../etc/passwd");
        let err = validate_path(std::slice::from_ref(&root), &evil).unwrap_err();
        assert!(matches!(err, ToolError::PathTraversal(_)));
    }

    #[test]
    fn validate_path_accepts_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f = root.join("file.txt");
        std::fs::write(&f, "x").unwrap();
        let ok = validate_path(std::slice::from_ref(&root), &f).unwrap();
        assert!(ok.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn validate_path_accepts_nonexistent_target_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let target = root.join("new/deep/file.txt");
        let resolved = validate_path(std::slice::from_ref(&root), &target).unwrap();
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        assert!(resolved.ends_with("new/deep/file.txt"));
    }

    #[test]
    fn validate_path_rejects_dotdot_escape_on_create_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let target = root.join("../evil.txt");
        let err = validate_path(std::slice::from_ref(&root), &target).unwrap_err();
        assert!(matches!(err, ToolError::PathTraversal(_)));
    }

    #[test]
    fn validate_path_rejects_symlink_escaping_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let target = link.join("secret.txt");
        let err = validate_path(std::slice::from_ref(&root), &target).unwrap_err();
        assert!(matches!(err, ToolError::PathTraversal(_)));
    }

    #[test]
    fn bound_output_truncates_and_reports_original() {
        let big = "a".repeat(10 * 1024 * 1024);
        let (out, orig) = bound_output(&big, 1024);
        assert_eq!(orig, Some(10 * 1024 * 1024));
        assert!(out.len() <= 1024 + 32);
        assert!(out.contains("elided"));
    }

    #[test]
    fn bound_output_passthrough_when_small() {
        let (out, orig) = bound_output("small", 1024);
        assert_eq!(out, "small");
        assert!(orig.is_none());
    }

    #[tokio::test]
    async fn dummy_tool_error_yields_toolresult_no_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        reg.register(Box::new(DummyErrTool));
        let res = reg
            .dispatch(
                "dummy",
                Value::Null,
                CancellationToken::new(),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Error);
        assert_eq!(res.error.unwrap().code, "boom");
    }

    #[tokio::test]
    async fn unknown_tool_yields_structured_error_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let res = reg
            .dispatch(
                "nope",
                Value::Null,
                CancellationToken::new(),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Error);
        assert_eq!(res.error.unwrap().code, "unknown_tool");
    }

    #[test]
    fn tool_result_denied_shape_matches_spec_12_2() {
        let r = ToolResult {
            status: ToolStatus::Denied,
            output: String::new(),
            truncated: None,
            error: Some(ToolErrorObj {
                code: "plugin_denied".into(),
                plugin_id: Some("destructive-command-gate".into()),
                message: "blocked".into(),
            }),
            metadata: Value::Null,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["status"], "denied");
        assert_eq!(json["error"]["code"], "plugin_denied");
        assert_eq!(json["error"]["plugin_id"], "destructive-command-gate");
        assert_eq!(json["error"]["message"], "blocked");
    }
}
