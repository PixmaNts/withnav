//! The permission gate: which tool calls need a driver's approval, what an
//! approval covers, and the grants an approval can leave behind.
//!
//! `write`, `edit`, `bash` and `fetch` are gated for a session that runs as an
//! agent; `read`, `grep`, `glob`, `skill` and `ask` never are. A gated call is
//! described by a [`Scope`]: the folder it changes, or the origin it reaches.
//! The driver answers with a [`PermissionDecision`]; an "allow folder" answer
//! is remembered, in memory only, as a grant keyed by agent, capability and
//! scope.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::agent::definitions::AgentDefinition;
use crate::error::ToolError;
use crate::provider::ToolCall;
use crate::tools::{bound_output, validate_path, ToolContext};

/// The longest rendering of a call's arguments kept in a request, in bytes.
pub const MAX_REVIEW_ARGUMENT_BYTES: usize = 4 * 1024;

/// A driver's answer to a [`PermissionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run this one call.
    AllowOnce,
    /// Run this call and every later call of the same capability inside the
    /// same scope, for this agent, until the session ends.
    AllowFolder,
    /// Do not run the call; `message` is what the model reads back.
    Deny { message: String },
}

/// What a gated call touches.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Scope {
    /// A canonical folder. A grant on it covers its descendants.
    Folder(PathBuf),
    /// A URL origin: `scheme://host[:port]`.
    Origin(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Folder(path) => write!(f, "{}", path.display()),
            Self::Origin(origin) => f.write_str(origin),
        }
    }
}

/// A gated call waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: Ulid,
    pub tool_call_id: String,
    pub tool: String,
    /// The call's arguments as JSON text, cut to
    /// [`MAX_REVIEW_ARGUMENT_BYTES`] for review.
    pub arguments: String,
    pub scope: Scope,
    /// The id of the agent that made the call.
    pub agent: String,
    /// Why approval is needed, in a sentence a driver can show.
    pub reason: String,
}

impl PermissionRequest {
    pub fn new(call: &ToolCall, scope: Scope, agent: &str) -> Self {
        let arguments = serde_json::to_string(&call.args).unwrap_or_default();
        let (arguments, _) = bound_output(&arguments, MAX_REVIEW_ARGUMENT_BYTES);
        let reason = match &scope {
            Scope::Folder(folder) => format!("`{}` acts in {}", call.name, folder.display()),
            Scope::Origin(origin) => format!("`{}` reaches {origin}", call.name),
        };
        Self {
            id: Ulid::new(),
            tool_call_id: call.id.clone(),
            tool: call.name.clone(),
            arguments,
            scope,
            agent: agent.to_string(),
            reason,
        }
    }
}

/// Why a decision was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PermissionError {
    /// No permission request with that id is waiting: it was already
    /// answered, it was settled by a stop or a new message, or it never
    /// existed.
    #[error("no such permission request is waiting")]
    Stale,
}

/// What a grant allows: `write` and `edit` share one capability, `bash` and
/// `fetch` each have their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Capability {
    Files,
    Bash,
    Fetch,
}

impl Capability {
    fn of(tool: &str) -> Option<Self> {
        match tool {
            "write" | "edit" => Some(Self::Files),
            "bash" => Some(Self::Bash),
            "fetch" => Some(Self::Fetch),
            _ => None,
        }
    }
}

/// The approvals remembered for a session. Memory only: never persisted, so a
/// restart starts without any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    entries: Vec<(String, Capability, Scope)>,
}

impl Grants {
    /// Remembers that `agent` may use `tool`'s capability inside `scope`.
    /// Tools that are never gated record nothing.
    pub fn grant(&mut self, agent: &str, tool: &str, scope: Scope) {
        let Some(capability) = Capability::of(tool) else {
            return;
        };
        let entry = (agent.to_string(), capability, scope);
        if !self.entries.contains(&entry) {
            self.entries.push(entry);
        }
    }

    /// Whether a grant already covers `tool` acting in `scope` for `agent`.
    /// A folder grant covers descendants, but never reaches into a writable
    /// root it does not itself sit in: a grant on the workspace does not
    /// cover the config directory, wherever that lives.
    pub fn covers(&self, agent: &str, tool: &str, scope: &Scope, cx: &ToolContext) -> bool {
        let Some(capability) = Capability::of(tool) else {
            return true;
        };
        self.entries.iter().any(|(who, cap, granted)| {
            who == agent
                && *cap == capability
                && match (granted, scope) {
                    (Scope::Folder(grant), Scope::Folder(target)) => {
                        folder_covers(grant, target, cx)
                    }
                    (Scope::Origin(grant), Scope::Origin(target)) => grant == target,
                    _ => false,
                }
        })
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn folder_covers(grant: &Path, target: &Path, cx: &ToolContext) -> bool {
    if !target.starts_with(grant) {
        return false;
    }
    let workspace = canonical(&cx.workspace);
    cx.writable_roots
        .iter()
        .map(|root| canonical(root))
        .filter(|root| *root != workspace)
        .all(|root| !target.starts_with(&root) || grant.starts_with(&root))
}

/// What a gated call needs, as decided before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needed {
    pub scope: Scope,
    /// For `write` and `edit`: the canonical file the call will change, as
    /// resolved when the request was made. An approval covers this file only;
    /// a call that resolves elsewhere when it runs needs a new approval.
    pub destination: Option<PathBuf>,
}

/// Whether `role` is checked by the role guard: `plan` is. Every other agent
/// runs under the ordinary permission gate alone.
pub fn is_guarded(role: &AgentDefinition) -> bool {
    role.id.as_str() == crate::agent::definitions::PLAN
}

/// The decision that is applied for `agent`: a guarded agent's exceptions are
/// offered once or denied, never "allow folder", so that answer counts as a
/// single approval.
pub fn offered_decision(agent: &str, decision: PermissionDecision) -> PermissionDecision {
    if agent == crate::agent::definitions::PLAN && decision == PermissionDecision::AllowFolder {
        PermissionDecision::AllowOnce
    } else {
        decision
    }
}

/// Whether a guarded agent may run `tool` on `needed` without the guard and
/// without a request: a `write` or `edit` whose RESOLVED destination is a
/// regular `.md` file (a plan's normal job). The destination is the canonical
/// path `validate_path` produced, so a `.md` name that is a symlink to
/// another file is not exempt, and neither is a link that dangles: writing
/// through it would create its target.
pub fn exempt_from_guard(tool: &str, needed: &Needed) -> bool {
    if !matches!(tool, "write" | "edit") {
        return false;
    }
    let Some(destination) = needed.destination.as_deref() else {
        return false;
    };
    let markdown = destination
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    let link = std::fs::symlink_metadata(destination)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    markdown && !link
}

/// What the gate decides about a call, with no side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The call runs without approval.
    Ungated,
    /// The call is refused before it runs; the string is the tool error the
    /// model reads back.
    Rejected(String),
    /// The call waits for approval.
    Needs(Needed),
}

/// What a call needs approval for.
///
/// * `write`, `edit`: the canonical parent folder of the validated path, and
///   the canonical file itself. A missing path argument or a path
///   `validate_path` refuses is [`Gate::Rejected`]: the tool never runs.
/// * `bash`: the working directory, which is the workspace. This is the scope
///   the driver approves, NOT a confinement: the command can touch anything
///   the process can.
/// * `fetch`: the URL's origin. An unusable URL needs no approval: the tool
///   will refuse it without side effect.
/// * everything else, and every call of a bare runtime (`role` is `None`):
///   never.
pub fn needs_permission(
    role: Option<&AgentDefinition>,
    tool: &str,
    args: &Value,
    cx: &ToolContext,
) -> Gate {
    if role.is_none() {
        return Gate::Ungated;
    }
    match tool {
        "write" | "edit" => {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return Gate::Rejected(
                    ToolError::Denied {
                        code: "invalid_args".into(),
                        message: "missing string argument: path".into(),
                    }
                    .to_string(),
                );
            };
            let resolved = match validate_path(&cx.writable_roots, Path::new(path)) {
                Ok(resolved) => resolved,
                Err(e) => return Gate::Rejected(e.to_string()),
            };
            let Some(parent) = resolved.parent() else {
                return Gate::Rejected(ToolError::PathTraversal(resolved.clone()).to_string());
            };
            Gate::Needs(Needed {
                scope: Scope::Folder(parent.to_path_buf()),
                destination: Some(resolved),
            })
        }
        "bash" => Gate::Needs(Needed {
            scope: Scope::Folder(canonical(&cx.workspace)),
            destination: None,
        }),
        "fetch" => {
            let Some(url) = args
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| reqwest::Url::parse(url.trim()).ok())
            else {
                return Gate::Ungated;
            };
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Gate::Ungated;
            }
            Gate::Needs(Needed {
                scope: Scope::Origin(url.origin().ascii_serialization()),
                destination: None,
            })
        }
        _ => Gate::Ungated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::definitions::Agents;
    use serde_json::json;

    struct Fixture {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        config: PathBuf,
        cx: ToolContext,
        role: std::sync::Arc<AgentDefinition>,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ws = root.join("ws");
        let config = root.join("config");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let cx = ToolContext {
            workspace: ws.clone(),
            writable_roots: vec![ws.clone(), config.clone()],
            readable_roots: Vec::new(),
        };
        let role = Agents::builtin().get("build").unwrap().clone();
        Fixture {
            _tmp: tmp,
            ws,
            config,
            cx,
            role,
        }
    }

    fn gate(f: &Fixture, tool: &str, args: Value) -> Gate {
        needs_permission(Some(&f.role), tool, &args, &f.cx)
    }

    /// The scope of a call that needs approval, `None` for any other gate.
    fn need(f: &Fixture, tool: &str, args: Value) -> Option<Scope> {
        match gate(f, tool, args) {
            Gate::Needs(needed) => Some(needed.scope),
            _ => None,
        }
    }

    #[test]
    fn write_and_edit_need_the_canonical_parent_folder() {
        let f = fixture();
        let file = f.ws.join("src/main.rs");
        let args = json!({"path": file, "content": "x"});
        assert_eq!(
            need(&f, "write", args.clone()),
            Some(Scope::Folder(f.ws.join("src")))
        );
        assert_eq!(
            need(&f, "edit", args),
            Some(Scope::Folder(f.ws.join("src")))
        );
        // A folder that does not exist yet still resolves under its root.
        let deep = json!({"path": f.ws.join("new/deeper/a.txt")});
        assert_eq!(
            need(&f, "write", deep),
            Some(Scope::Folder(f.ws.join("new/deeper")))
        );
    }

    #[test]
    fn a_symlink_out_of_the_roots_is_rejected_not_ungated() {
        let f = fixture();
        let outside = f.ws.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, f.ws.join("link")).unwrap();
        assert!(matches!(
            gate(&f, "write", json!({"path": f.ws.join("link/a.txt")})),
            Gate::Rejected(_)
        ));
    }

    #[test]
    fn a_needed_write_names_the_canonical_destination() {
        let f = fixture();
        let file = f.ws.join("src/main.rs");
        let Gate::Needs(needed) = gate(&f, "write", json!({"path": file})) else {
            panic!("a write inside the roots needs approval");
        };
        assert_eq!(needed.destination, Some(file));
        let Gate::Needs(bash) = gate(&f, "bash", json!({"command": "ls"})) else {
            panic!("bash needs approval");
        };
        assert_eq!(bash.destination, None);
    }

    #[test]
    fn paths_the_tool_would_refuse_are_rejected_and_unusable_urls_need_nothing() {
        let f = fixture();
        for args in [
            json!({"path": "/etc/passwd"}),
            json!({}),
            json!({"path": 3}),
        ] {
            assert!(matches!(gate(&f, "write", args.clone()), Gate::Rejected(_)));
            assert!(matches!(gate(&f, "edit", args), Gate::Rejected(_)));
        }
        assert_eq!(
            gate(&f, "fetch", json!({"url": "not a url"})),
            Gate::Ungated
        );
        assert_eq!(
            gate(&f, "fetch", json!({"url": "file:///etc/passwd"})),
            Gate::Ungated
        );
    }

    #[test]
    fn bash_needs_the_workspace_and_fetch_the_origin() {
        let f = fixture();
        assert_eq!(
            need(&f, "bash", json!({"command": "ls"})),
            Some(Scope::Folder(f.ws.clone()))
        );
        assert_eq!(
            need(&f, "fetch", json!({"url": "https://Example.com/a/b?q=1"})),
            Some(Scope::Origin("https://example.com".into()))
        );
        assert_eq!(
            need(&f, "fetch", json!({"url": "http://localhost:8080/x"})),
            Some(Scope::Origin("http://localhost:8080".into()))
        );
    }

    #[test]
    fn reading_tools_and_a_bare_runtime_never_need_permission() {
        let f = fixture();
        for tool in ["read", "grep", "glob", "skill", "ask"] {
            assert_eq!(
                need(&f, tool, json!({"path": f.ws.join("a"), "command": "ls"})),
                None,
                "{tool}"
            );
        }
        let args = json!({"command": "ls"});
        assert_eq!(needs_permission(None, "bash", &args, &f.cx), Gate::Ungated);
        assert_eq!(
            needs_permission(None, "write", &json!({}), &f.cx),
            Gate::Ungated
        );
    }

    #[test]
    fn the_request_bounds_the_arguments_it_shows() {
        let f = fixture();
        let call = ToolCall {
            id: "c1".into(),
            name: "write".into(),
            args: json!({"path": f.ws.join("a"), "content": "x".repeat(50_000)}),
            args_valid: true,
        };
        let request = PermissionRequest::new(&call, Scope::Folder(f.ws.clone()), "build");
        assert!(request.arguments.len() <= MAX_REVIEW_ARGUMENT_BYTES);
        assert_eq!(request.tool_call_id, "c1");
        assert_eq!(request.agent, "build");
        assert!(request.reason.contains("write"));
    }

    #[test]
    fn a_folder_grant_covers_descendants_for_write_and_edit_alike() {
        let f = fixture();
        let mut grants = Grants::default();
        grants.grant("build", "write", Scope::Folder(f.ws.clone()));
        let nested = Scope::Folder(f.ws.join("src/deep"));
        assert!(grants.covers("build", "write", &nested, &f.cx));
        assert!(grants.covers("build", "edit", &nested, &f.cx));
        assert!(grants.covers("build", "write", &Scope::Folder(f.ws.clone()), &f.cx));
    }

    #[test]
    fn a_grant_is_bound_to_its_agent_capability_and_folder() {
        let f = fixture();
        let mut grants = Grants::default();
        grants.grant("build", "write", Scope::Folder(f.ws.join("src")));
        let src = Scope::Folder(f.ws.join("src"));
        assert!(!grants.covers("plan", "write", &src, &f.cx));
        assert!(!grants.covers("build", "bash", &src, &f.cx));
        assert!(!grants.covers("build", "write", &Scope::Folder(f.ws.clone()), &f.cx));
        assert!(!grants.covers(
            "build",
            "write",
            &Scope::Folder(f.ws.join("srcother")),
            &f.cx
        ));
        // Ungated tools record nothing.
        grants.grant("build", "read", Scope::Folder(f.ws.clone()));
        assert_eq!(grants.entries.len(), 1);
    }

    #[test]
    fn bash_and_fetch_grants_are_their_own() {
        let f = fixture();
        let mut grants = Grants::default();
        let ws = Scope::Folder(f.ws.clone());
        grants.grant("build", "bash", ws.clone());
        assert!(grants.covers("build", "bash", &ws, &f.cx));
        assert!(!grants.covers("build", "write", &ws, &f.cx));

        let origin = Scope::Origin("https://example.com".into());
        grants.grant("build", "fetch", origin.clone());
        assert!(grants.covers("build", "fetch", &origin, &f.cx));
        assert!(!grants.covers(
            "build",
            "fetch",
            &Scope::Origin("https://example.com:8443".into()),
            &f.cx
        ));
        assert!(!grants.covers(
            "build",
            "fetch",
            &Scope::Origin("http://example.com".into()),
            &f.cx
        ));
    }

    #[test]
    fn a_workspace_grant_never_covers_the_config_directory() {
        let f = fixture();
        let mut grants = Grants::default();
        grants.grant("build", "write", Scope::Folder(f.ws.clone()));
        assert!(!grants.covers("build", "write", &Scope::Folder(f.config.clone()), &f.cx));

        // Even when the config directory lives inside the workspace.
        let inner = f.ws.join("cfg");
        std::fs::create_dir_all(&inner).unwrap();
        let cx = ToolContext {
            workspace: f.ws.clone(),
            writable_roots: vec![f.ws.clone(), inner.clone()],
            readable_roots: Vec::new(),
        };
        assert!(!grants.covers("build", "write", &Scope::Folder(inner.join("agents")), &cx));
        assert!(grants.covers("build", "write", &Scope::Folder(f.ws.join("src")), &cx));

        // A grant on the config directory itself covers it.
        grants.grant("build", "write", Scope::Folder(inner.clone()));
        assert!(grants.covers("build", "write", &Scope::Folder(inner.join("agents")), &cx));
    }
}
