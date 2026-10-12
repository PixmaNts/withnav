//! The permission gate: which tool calls need a Captain's approval, what an
//! approval covers, and the grants an approval can leave behind.
//!
//! `write`, `edit`, `bash` and `fetch` are gated for a session that runs as an
//! agent; outside-root or sensitive `read`, `grep` and `glob` calls are reviewed
//! too. Ordinary inside-root reads, `skill` and `ask` are ungated. A gated call is
//! described by a [`Scope`]: the folder it changes, or the origin it reaches.
//! The captain answers with a [`PermissionDecision`]; an "allow folder" answer
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOrigin {
    Captain,
    Bypass,
}

/// A Captain's answer to a [`PermissionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run this one call.
    AllowOnce,
    /// Run this call and every later call of the same capability inside the
    /// same scope, for this agent, until the session ends.
    AllowFolder,
    /// Skip the Captain-only configuration hold until this root session ends.
    AllowConfigSession,
    AllowSkillMachine,
    /// Do not run the call; `message` is what the model reads back.
    Deny {
        message: String,
    },
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestOrigin {
    #[default]
    Ordinary,
    Flow {
        flow: String,
        run_id: String,
    },
}

/// A gated call waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    #[serde(default)]
    pub origin: RequestOrigin,
    #[serde(default)]
    pub flow_regates: usize,
    #[serde(default)]
    pub requires_captain: bool,
    #[serde(default)]
    pub allow_config_session: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_script: Option<Box<super::skill_grants::SkillScript>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_gate: Option<Box<Value>>,
    pub id: Ulid,
    pub tool_call_id: String,
    pub tool: String,
    /// The call's arguments as JSON text, cut to
    /// [`MAX_REVIEW_ARGUMENT_BYTES`] for review.
    pub arguments: String,
    pub scope: Scope,
    /// The id of the agent that made the call.
    pub agent: String,
    /// Why approval is needed, in a sentence a captain can show.
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
            origin: RequestOrigin::Ordinary,
            flow_regates: 0,
            requires_captain: false,
            allow_config_session: false,
            skill_script: None,
            permission_gate: None,
            id: Ulid::generate(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

type Entry = (String, Capability, Scope);

/// The approvals remembered for a session. In memory by default; one built
/// with [`Grants::persisted`] loads and saves those of its workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    entries: Vec<Entry>,
    store: Option<GrantStore>,
}

/// Where a workspace's grants are kept: one JSON file for every workspace,
/// keyed by the canonical workspace path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GrantStore {
    file: PathBuf,
    workspace: String,
}

type GrantFile = std::collections::BTreeMap<String, Vec<Entry>>;

impl GrantStore {
    /// The whole file; an absent file is empty. An unreadable or malformed one
    /// is an error, so it is never overwritten.
    fn read(&self) -> Result<GrantFile, String> {
        match std::fs::read(&self.file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GrantFile::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Adds `entries` to this workspace's saved grants, keeping those another
    /// session saved meanwhile: temp file (0600), sync, rename. The whole
    /// read-merge-rename runs under an exclusive advisory lock on a sibling
    /// file: the rename alone keeps readers from seeing a torn file, but two
    /// unlocked writers both read the old content and the later rename drops
    /// the other's grant.
    #[cfg(unix)]
    fn save(&self, entries: &[Entry]) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // The lock is on its own file, not on `grants.json`, whose inode the
        // rename replaces. It is released when the guard drops.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.file.with_extension("json.lock"))
            .map_err(|e| e.to_string())?;
        let _lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| e.to_string())?;
        let mut file = self.read()?;
        let saved = file.entry(self.workspace.clone()).or_default();
        for entry in entries {
            if !saved.contains(entry) {
                saved.push(entry.clone());
            }
        }
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        let tmp = self
            .file
            .with_extension(format!("{}.tmp", Ulid::generate()));
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        out.write_all(&json)
            .and_then(|()| out.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())
    }
}

impl Grants {
    /// The grants kept in `file` for `workspace`, saved back there on every
    /// new grant. A file that cannot be read starts the session without any
    /// (and is left as it is).
    pub fn persisted(file: PathBuf, workspace: &Path) -> Self {
        let store = GrantStore {
            file,
            workspace: canonical(workspace).to_string_lossy().into_owned(),
        };
        let entries = match store.read() {
            Ok(mut all) => all.remove(&store.workspace).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(file = %store.file.display(), "cannot read the saved grants: {err}");
                Vec::new()
            }
        };
        Self {
            entries,
            store: Some(store),
        }
    }

    /// Remembers that `agent` may use `tool`'s capability inside `scope`.
    /// Tools that are never gated record nothing.
    pub fn grant(&mut self, agent: &str, tool: &str, scope: Scope) {
        let Some(capability) = Capability::of(tool) else {
            return;
        };
        let entry = (agent.to_string(), capability, scope);
        if self.entries.contains(&entry) {
            return;
        }
        self.entries.push(entry);
        // A failed save keeps the grant for this session only.
        if let Some(store) = &self.store {
            if let Err(err) = store.save(&self.entries) {
                tracing::warn!(file = %store.file.display(), "cannot save the grants: {err}");
            }
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
    let Ok(resolved_grant) = grant.canonicalize() else {
        return false;
    };
    if resolved_grant != grant {
        return false;
    }
    let Ok(resolved_target) = validate_path(&[resolved_grant], target) else {
        return false;
    };
    let Ok(workspace) = cx.workspace.canonicalize() else {
        return false;
    };
    cx.writable_roots.iter().all(|root| {
        // Canonical resolution adds checks; it must not discard the original
        // protected-root boundary when a spelling traverses out of that root.
        let previous_root = canonical(root);
        if previous_root != workspace
            && target.starts_with(&previous_root)
            && !grant.starts_with(&previous_root)
        {
            return false;
        }
        resolve_protected_root(root).is_ok_and(|root| {
            root == workspace || !resolved_target.starts_with(&root) || grant.starts_with(&root)
        })
    })
}

/// What a gated call needs, as decided before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needed {
    pub scope: Scope,
    /// For native file tools: the canonical target the call will access, as
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
pub fn offered_decision(
    agent: &str,
    tool: &str,
    decision: PermissionDecision,
) -> PermissionDecision {
    if (agent == crate::agent::definitions::PLAN
        || matches!(tool, "bash" | "write" | "edit" | "read" | "grep" | "glob"))
        && decision == PermissionDecision::AllowFolder
    {
        PermissionDecision::AllowOnce
    } else {
        decision
    }
}

/// Whether a guarded agent may run `tool` on `needed` without the guard and
/// without a request: a `write` or `edit` of a markdown file, in the plan's
/// normal course of work. All of these must hold, so that nothing but a plain
/// markdown file is ever exempt:
///
/// * the RESOLVED destination ends in `.md`;
/// * the path as the model wrote it is not a link (any kind, dangling or
///   not): the check is on the un-canonicalized path, whose resolution would
///   hide a link to another `.md` file or to a file that is not markdown;
///   a link in any folder of the path below the project and the writable
///   roots counts too, since it would carry the write elsewhere;
/// * the destination is an existing regular file (not a directory, a FIFO or
///   a device), or a missing one in a folder that already exists: a write
///   that would create folders is an exception.
pub fn exempt_from_guard(tool: &str, args: &Value, needed: &Needed, cx: &ToolContext) -> bool {
    if !matches!(tool, "write" | "edit") {
        return false;
    }
    let (Some(destination), Some(path)) = (
        needed.destination.as_deref(),
        args.get("path").and_then(Value::as_str),
    ) else {
        return false;
    };
    if !destination
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        return false;
    }
    let Ok(requested) = requested_path(Path::new(path), cx) else {
        return false;
    };
    if has_link(&requested, cx) || has_link(destination, cx) {
        return false;
    }
    match std::fs::symlink_metadata(destination) {
        Ok(meta) => meta.file_type().is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            destination.parent().is_some_and(|parent| parent.is_dir())
        }
        Err(_) => false,
    }
}

/// Whether `path` or any folder above it is a link, ignoring the folders at or
/// above the workspace and the writable roots (as written or canonical): those
/// are the host's own layout (`/home` may itself be a link), not the model's.
fn has_link(path: &Path, cx: &ToolContext) -> bool {
    let trusted: Vec<PathBuf> = std::iter::once(&cx.workspace)
        .chain(cx.writable_roots.iter())
        .flat_map(|root| [Some(root.clone()), root.canonicalize().ok()])
        .flatten()
        .collect();
    path.ancestors()
        .filter(|ancestor| !trusted.iter().any(|root| root.starts_with(ancestor)))
        .any(is_link)
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn review_destination(path: &Path, cx: &ToolContext) -> Result<PathBuf, ToolError> {
    let resolved = resolve_read_target(path, cx)?;
    if resolved.is_dir() {
        return Err(ToolError::Denied {
            code: "invalid_path".into(),
            message: "destination is a directory".into(),
        });
    }
    Ok(resolved)
}

#[cfg(unix)]
pub fn resolve_protected_root(path: &Path) -> Result<PathBuf, ToolError> {
    validate_path(&[PathBuf::from("/")], path)
}

/// Whether `args`' path still resolves to `destination`, the canonical file a
/// call was checked for, and that file is not a link now. A call whose file
/// moved between the check and the dispatch (a link put in its place) must
/// not run on the old verdict.
pub fn still_resolves(args: &Value, destination: Option<&Path>, cx: &ToolContext) -> bool {
    let Some(destination) = destination else {
        return true;
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap_or(&cx.workspace);
    resolve_read_target(path, cx).is_ok_and(|now| now == destination) && !is_link(destination)
}

pub fn resolve_read_target(path: &Path, cx: &ToolContext) -> Result<PathBuf, ToolError> {
    let path = requested_path(path, cx)?;
    resolve_protected_root(&path)
}

pub fn requested_path(path: &Path, cx: &ToolContext) -> Result<PathBuf, ToolError> {
    // Native tools do not perform shell expansion: only a bare `~` component
    // names HOME; `~name` remains a literal workspace-relative filename.
    let path = if path.starts_with("~") {
        let home =
            std::env::var_os("HOME").ok_or_else(|| ToolError::PathTraversal(path.to_path_buf()))?;
        PathBuf::from(home).join(
            path.strip_prefix("~")
                .map_err(|_| ToolError::PathTraversal(path.to_path_buf()))?,
        )
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        cx.workspace.join(path)
    };
    if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
        return Err(ToolError::PathTraversal(path));
    }
    Ok(path)
}

/// What the gate decides about a call, with no side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The call runs without approval.
    Ungated,
    Bound(PathBuf),
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
///   the Captain approves, NOT a confinement: the command can touch anything
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
        "read" | "grep" | "glob" => {
            let path = match args.get("path") {
                Some(Value::String(path)) if !path.is_empty() => PathBuf::from(path),
                None if tool != "read" => cx.workspace.clone(),
                _ => return Gate::Rejected("missing string argument: path".into()),
            };
            let resolved = match resolve_read_target(&path, cx) {
                Ok(path) => path,
                Err(error) => return Gate::Rejected(error.to_string()),
            };
            let inside = validate_path(&cx.read_roots(), &resolved).is_ok();
            let path = match requested_path(&path, cx) {
                Ok(path) => path,
                Err(error) => return Gate::Rejected(error.to_string()),
            };
            if tool == "read" && resolved.is_dir() {
                return Gate::Rejected("read requires a specific file, not a directory".into());
            }
            if tool != "read" {
                if super::permission_gate::sensitive_read(&path)
                    || super::permission_gate::sensitive_read(&resolved)
                {
                    return Gate::Rejected("sensitive search root: use read on individual files; each read is reviewed".into());
                }
                return if inside {
                    Gate::Bound(resolved)
                } else {
                    Gate::Rejected(crate::tools::OUTSIDE_SEARCH.into())
                };
            }
            if inside
                && !super::permission_gate::sensitive_read(&resolved)
                && !super::permission_gate::sensitive_read(&path)
            {
                return Gate::Bound(resolved);
            }
            Gate::Needs(Needed {
                scope: Scope::Folder(resolved.parent().unwrap_or(&resolved).to_path_buf()),
                destination: Some(resolved),
            })
        }
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
            let resolved = match review_destination(Path::new(path), cx) {
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
            binding: None,
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

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_roots_is_rejected_not_ungated() {
        let f = fixture();
        let outside = f.ws.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, f.ws.join("link")).unwrap();
        assert!(matches!(
            gate(&f, "write", json!({"path": f.ws.join("link/a.txt")})),
            Gate::Needs(_)
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
        if let Some(home) = std::env::var_os("HOME") {
            for (path, expected) in [
                ("~", PathBuf::from(&home)),
                ("~/x", PathBuf::from(&home).join("x")),
            ] {
                assert_eq!(requested_path(Path::new(path), &f.cx).unwrap(), expected);
            }
        }
        for path in [
            "~root/x",
            "~pixma/.config/withnav/x",
            "~other",
            "~+/x",
            "~-/x",
            "~name.txt",
        ] {
            let literal = f.ws.join(path);
            std::fs::create_dir_all(literal.parent().unwrap()).unwrap();
            std::fs::write(&literal, "ordinary workspace file").unwrap();
            assert_eq!(requested_path(Path::new(path), &f.cx).unwrap(), literal);
            for tool in ["read", "grep", "glob"] {
                assert_eq!(
                    gate(&f, tool, json!({"path": path})),
                    Gate::Bound(literal.clone()),
                    "{tool} {path}"
                );
            }
            for tool in ["write", "edit"] {
                assert!(
                    matches!(gate(&f, tool, json!({"path": path})), Gate::Needs(needed) if needed.destination == Some(literal.clone())),
                    "{tool} {path}"
                );
            }
        }
        for args in [json!({}), json!({"path": 3})] {
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

    #[cfg(unix)]
    #[test]
    fn a_persisted_grant_survives_a_new_session_in_the_same_workspace_only() {
        let f = fixture();
        let file = f.config.join("grants.json");
        let mut first = Grants::persisted(file.clone(), &f.ws);
        first.grant("build", "bash", Scope::Folder(f.ws.clone()));
        let second = Grants::persisted(file.clone(), &f.ws);
        assert!(second.covers("build", "bash", &Scope::Folder(f.ws.clone()), &f.cx));
        assert!(!second.covers("plan", "bash", &Scope::Folder(f.ws.clone()), &f.cx));
        let other = Grants::persisted(file.clone(), &f.ws.join("src"));
        assert!(!other.covers("build", "bash", &Scope::Folder(f.ws.clone()), &f.cx));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Given several sessions of one workspace granting distinct folders at once
    // When each saves through its own `Grants`
    // Then the file holds every grant: no concurrent save drops another's
    #[test]
    fn concurrent_grants_from_several_sessions_are_all_kept() {
        const SESSIONS: usize = 8;
        const PER_SESSION: usize = 6;
        let f = fixture();
        let file = f.config.join("grants.json");
        let start = std::sync::Barrier::new(SESSIONS);
        std::thread::scope(|scope| {
            for session in 0..SESSIONS {
                let (file, ws, start) = (&file, &f.ws, &start);
                scope.spawn(move || {
                    let mut grants = Grants::persisted(file.clone(), ws);
                    start.wait();
                    for n in 0..PER_SESSION {
                        let folder = ws.join(format!("s{session}-{n}"));
                        grants.grant("build", "bash", Scope::Folder(folder));
                    }
                });
            }
        });
        let saved = Grants::persisted(file, &f.ws);
        assert_eq!(saved.entries.len(), SESSIONS * PER_SESSION);
    }

    // Given a save that waits on the lock another writer holds
    // When that writer commits its own grant and then lets go
    // Then the waiting save merges it instead of replacing the file
    #[cfg(unix)]
    #[test]
    fn a_save_waits_for_the_lock_and_keeps_what_the_holder_wrote() {
        let f = fixture();
        let file = f.config.join("grants.json");
        let holder = GrantStore {
            file: file.clone(),
            workspace: canonical(&f.ws).to_string_lossy().into_owned(),
        };
        let lock = std::fs::File::create(file.with_extension("json.lock")).unwrap();
        let guard = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| e)
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let waiting = {
            let file = file.clone();
            let ws = f.ws.clone();
            std::thread::spawn(move || {
                let mut grants = Grants::persisted(file, &ws);
                started_tx.send(()).unwrap();
                grants.grant("build", "bash", Scope::Folder(ws.join("waiting")));
            })
        };
        started_rx.recv().unwrap();
        // The holder's write lands while the other save is parked (or about to be).
        let theirs: Entry = (
            "build".into(),
            Capability::Bash,
            Scope::Folder(f.ws.join("holder")),
        );
        let mut all = GrantFile::new();
        all.insert(holder.workspace.clone(), vec![theirs.clone()]);
        std::fs::write(&file, serde_json::to_vec(&all).unwrap()).unwrap();
        drop(guard);
        waiting.join().unwrap();
        let saved = holder.read().unwrap().remove(&holder.workspace).unwrap();
        assert!(saved.contains(&theirs), "{saved:?}");
        assert_eq!(saved.len(), 2, "{saved:?}");
    }

    #[test]
    fn a_malformed_grants_file_is_never_overwritten() {
        let f = fixture();
        let file = f.config.join("grants.json");
        std::fs::write(&file, "not json").unwrap();
        let mut grants = Grants::persisted(file.clone(), &f.ws);
        grants.grant("build", "bash", Scope::Folder(f.ws.clone()));
        assert!(grants.covers("build", "bash", &Scope::Folder(f.ws.clone()), &f.cx));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_grant_covers_descendants_for_write_and_edit_alike() {
        let f = fixture();
        let mut grants = Grants::default();
        grants.grant("build", "write", Scope::Folder(f.ws.clone()));
        let nested = Scope::Folder(f.ws.join("src/deep"));
        assert!(grants.covers("build", "write", &nested, &f.cx));
        assert!(grants.covers("build", "edit", &nested, &f.cx));
        assert!(grants.covers("build", "write", &Scope::Folder(f.ws.clone()), &f.cx));
        std::os::unix::fs::symlink(&f.config, f.ws.join("outside-link")).unwrap();
        for (path, covered) in [
            (f.ws.join("missing/deep"), true),
            (f.ws.join("outside-link/missing"), false),
            (f.ws.join("../outside/missing"), false),
            (f.ws.join("missing/../../outside"), false),
        ] {
            assert_eq!(
                grants.covers("build", "write", &Scope::Folder(path.clone()), &f.cx),
                covered,
                "{}",
                path.display()
            );
        }
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
            binding: None,
            workspace: f.ws.clone(),
            writable_roots: vec![f.ws.clone(), inner.clone()],
            readable_roots: Vec::new(),
        };
        assert!(!grants.covers("build", "write", &Scope::Folder(inner.join("agents")), &cx));
        for path in [inner.join("../src"), inner.join("missing/../../src")] {
            assert!(!grants.covers("build", "write", &Scope::Folder(path), &cx));
        }
        assert!(grants.covers("build", "write", &Scope::Folder(f.ws.join("src")), &cx));

        // A grant on the config directory itself covers it.
        grants.grant("build", "write", Scope::Folder(inner.clone()));
        assert!(grants.covers("build", "write", &Scope::Folder(inner.join("agents")), &cx));
    }

    fn exempt(f: &Fixture, tool: &str, path: &Path) -> bool {
        let args = json!({"path": path, "content": "x"});
        match gate(f, tool, args.clone()) {
            Gate::Needs(needed) => exempt_from_guard(tool, &args, &needed, &f.cx),
            Gate::Rejected(_) => false,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn only_a_plain_markdown_file_is_exempt_from_the_guard() {
        let f = fixture();
        std::fs::write(f.ws.join("a.md"), "a").unwrap();
        assert!(exempt(&f, "write", &f.ws.join("a.md")), "an existing .md");
        assert!(exempt(&f, "edit", &f.ws.join("a.md")));
        assert!(exempt(&f, "write", &f.ws.join("new.MD")), "a new .md");
        assert!(!exempt(&f, "write", &f.ws.join("src/main.rs")));
        assert!(
            !exempt(&f, "write", &f.ws.join("deeper/new.md")),
            "creating a folder is an exception"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_final_component_is_never_exempt() {
        let f = fixture();
        std::fs::write(f.ws.join("a.md"), "a").unwrap();
        std::fs::write(f.ws.join("main.rs"), "fn main() {}").unwrap();
        std::os::unix::fs::symlink(f.ws.join("a.md"), f.ws.join("to_md.md")).unwrap();
        std::os::unix::fs::symlink(f.ws.join("main.rs"), f.ws.join("to_rs.md")).unwrap();
        std::os::unix::fs::symlink(f.ws.join("missing.md"), f.ws.join("dangling.md")).unwrap();
        for name in ["to_md.md", "to_rs.md", "dangling.md"] {
            assert!(!exempt(&f, "write", &f.ws.join(name)), "{name}");
            assert!(!exempt(&f, "edit", &f.ws.join(name)), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_a_parent_folder_is_never_exempt() {
        let f = fixture();
        // `notes` leads to another writable root, so the call is still a
        // request; only the link keeps it from being exempt.
        std::fs::write(f.config.join("spec.md"), "s").unwrap();
        std::os::unix::fs::symlink(&f.config, f.ws.join("notes")).unwrap();
        for tool in ["write", "edit"] {
            assert!(!exempt(&f, tool, &f.ws.join("notes/spec.md")), "{tool}");
            assert!(!exempt(&f, tool, &f.ws.join("notes/new.md")), "{tool} new");
        }
        // A link further up the path counts as well.
        std::fs::create_dir(f.config.join("deep")).unwrap();
        assert!(!exempt(&f, "write", &f.ws.join("notes/deep/new.md")));
        // A relative path through the link is the same call.
        assert!(!exempt(&f, "write", Path::new("notes/spec.md")));
        // A real folder next to it stays exempt.
        std::fs::create_dir(f.ws.join("docs")).unwrap();
        assert!(exempt(&f, "write", &f.ws.join("docs/new.md")));
    }

    #[test]
    fn a_directory_or_a_pipe_named_like_markdown_is_not_exempt() {
        let f = fixture();
        std::fs::create_dir(f.ws.join("dir.md")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(f.ws.join("pipe.md"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!exempt(&f, "write", &f.ws.join("dir.md")));
        if made {
            assert!(!exempt(&f, "write", &f.ws.join("pipe.md")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_target_that_moved_no_longer_resolves() {
        let f = fixture();
        let file = f.ws.join("a.md");
        std::fs::write(&file, "a").unwrap();
        let args = json!({"path": file});
        assert!(still_resolves(&args, Some(&file), &f.cx));
        assert!(still_resolves(&args, None, &f.cx), "nothing to compare");
        std::fs::remove_file(&file).unwrap();
        std::fs::write(f.ws.join("b.rs"), "b").unwrap();
        std::os::unix::fs::symlink(f.ws.join("b.rs"), &file).unwrap();
        assert!(!still_resolves(&args, Some(&file), &f.cx));
        // Reviewed for the link's target as it now resolves: still the same file.
        assert!(still_resolves(&args, Some(&f.ws.join("b.rs")), &f.cx));
    }
}

#[cfg(windows)]
impl GrantStore {
    fn save(&self, _entries: &[Entry]) -> Result<(), String> {
        Err("Private permission grant persistence is not yet supported on Windows".into())
    }
}

#[cfg(windows)]
pub fn resolve_protected_root(_path: &Path) -> Result<PathBuf, ToolError> {
    Err(ToolError::Denied {
        code: "unsupported_platform".into(),
        message: "All-root permission resolution is not yet supported on Windows".into(),
    })
}
