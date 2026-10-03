//! Agent definitions: the roles a session can run as. A primary agent is the
//! one a driver talks to (`build`, `plan`, or a custom one); a subagent is
//! one another agent delegates to. A definition is a role prompt, an optional
//! model, a tool allow/deny map and an access level; it says what the agent
//! is for, not what it may execute.
//!
//! Sources, later ones replacing earlier ones that carry the same id:
//! the built-ins, `<home>/.config/opencode/agents/*.md` (only read),
//! `<navai config dir>/agents/*.md`, and `.navai/agents/*.md` of the project
//! (the worktree root down to the workspace, nearer directories last).
//! `build` and `plan` are reserved: a file with one of those names never
//! replaces the built-in and is reported.
//!
//! A file is `---` YAML header `---` and a markdown body that is the role
//! prompt. The header may hold `description`, `mode`
//! (`primary`, `subagent`, `all`; `all` when absent), `model`, `tools` (a map
//! of tool name to boolean) and `access` (`read-only` or `full`; `full` when
//! absent). Any other key is kept in `extra` and has no effect; permission
//! settings are reported as unsupported. A malformed header rejects the file
//! and is reported; the other files still load.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent::instructions::directories_from_root;
use crate::agent::skills::frontmatter;

/// The most agent files read from one directory.
pub const MAX_FILES: usize = 200;
/// The longest agent file read, in bytes.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
/// The most directory entries looked at in one directory.
const MAX_SCANNED_ENTRIES: usize = 10_000;
/// The longest agent id.
const MAX_ID_CHARS: usize = 64;

/// The id of the agent a session runs as when none is chosen.
pub const BUILD: &str = "build";
/// The id of the planning agent.
pub const PLAN: &str = "plan";

/// An agent's name: lowercase letters, digits, `-` and `_`, starting with a
/// letter or a digit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    /// Validates `raw` as an id.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut chars = raw.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            && raw.len() <= MAX_ID_CHARS;
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(format!(
                "`{raw}` is not a valid agent id (lowercase letters, digits, - and _, at most {MAX_ID_CHARS})"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The default agent.
    pub fn build() -> Self {
        Self(BUILD.to_string())
    }

    pub fn is_build(&self) -> bool {
        self.0 == BUILD
    }

    /// `build` and `plan`: names no file may take.
    pub fn is_reserved(&self) -> bool {
        self.0 == BUILD || self.0 == PLAN
    }
}

impl std::borrow::Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an agent can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// A driver can select it for a session.
    Primary,
    /// Another agent can delegate to it.
    Subagent,
    /// Both.
    All,
}

impl AgentMode {
    pub fn is_primary(self) -> bool {
        matches!(self, Self::Primary | Self::All)
    }

    pub fn is_subagent(self) -> bool {
        matches!(self, Self::Subagent | Self::All)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
            Self::All => "all",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "primary" => Ok(Self::Primary),
            "subagent" => Ok(Self::Subagent),
            "all" => Ok(Self::All),
            other => Err(format!(
                "`mode` must be primary, subagent or all, not `{other}`"
            )),
        }
    }
}

/// What an agent may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAccess {
    /// Reads and searches only: no write, edit or bash.
    ReadOnly,
    Full,
}

impl AgentAccess {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "read-only" => Ok(Self::ReadOnly),
            "full" => Ok(Self::Full),
            other => Err(format!("`access` must be read-only or full, not `{other}`")),
        }
    }
}

/// Where a definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    Builtin,
    /// `<home>/.config/opencode/agents`: only read.
    OpenCode(PathBuf),
    /// `<navai config dir>/agents`.
    User(PathBuf),
    /// `.navai/agents` of the project.
    Project(PathBuf),
}

/// One agent: its role and its limits.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub id: AgentId,
    pub description: String,
    pub mode: AgentMode,
    /// The role prompt (the file body); empty for none.
    pub prompt: String,
    /// The model alias this agent prefers, if any.
    pub model: Option<String>,
    /// Tool name to allowed: an explicit `false` removes a tool.
    pub tools: BTreeMap<String, bool>,
    pub access: AgentAccess,
    pub source: AgentSource,
    /// Header keys this version does not use, kept as written.
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// A problem found while loading, never fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// Where agent files are looked for beyond the built-ins.
#[derive(Debug, Clone, Default)]
pub struct AgentRoots {
    /// The home directory holding `.config/opencode/agents`.
    pub home: Option<PathBuf>,
    /// Navai's config directory holding `agents`.
    pub config_dir: Option<PathBuf>,
}

/// The agents of one session: a snapshot taken when it was created or
/// resumed.
#[derive(Debug, Clone, Default)]
pub struct Agents {
    by_id: BTreeMap<AgentId, Arc<AgentDefinition>>,
    diagnostics: Vec<Diagnostic>,
}

impl Agents {
    /// The built-ins alone.
    pub fn builtin() -> Self {
        let mut agents = Self::default();
        for definition in builtin_definitions() {
            agents
                .by_id
                .insert(definition.id.clone(), Arc::new(definition));
        }
        agents
    }

    /// The built-ins, then every source in precedence order. Blocking file
    /// I/O: call it from a blocking context.
    pub fn load(roots: &AgentRoots, workspace: &Path) -> Self {
        let mut agents = Self::builtin();
        if let Some(home) = &roots.home {
            let dir = home.join(".config").join("opencode").join("agents");
            agents.load_dir(&dir, None, AgentSource::OpenCode);
        }
        if let Some(config_dir) = &roots.config_dir {
            agents.load_dir(&config_dir.join("agents"), None, AgentSource::User);
        }
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        for dir in directories_from_root(&workspace) {
            // A project's agents stay inside the project: a link out of it
            // would let a repository pull a prompt from anywhere.
            agents.load_dir(
                &dir.join(".navai").join("agents"),
                Some(&dir),
                AgentSource::Project,
            );
        }
        agents
    }

    pub fn get(&self, id: &str) -> Option<&Arc<AgentDefinition>> {
        self.by_id.get(id)
    }

    /// Every agent, by id.
    pub fn all(&self) -> impl Iterator<Item = &Arc<AgentDefinition>> {
        self.by_id.values()
    }

    /// The agents a driver can select, by id.
    pub fn primary(&self) -> impl Iterator<Item = &Arc<AgentDefinition>> {
        self.all().filter(|a| a.mode.is_primary())
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Reads the agent files of `dir`. Every file, and the directory itself,
    /// must resolve (links followed) to a place inside `boundary`: the
    /// directory's own resolved path when `None`. Whatever leaves it is
    /// skipped with a diagnostic.
    fn load_dir(
        &mut self,
        dir: &Path,
        boundary: Option<&Path>,
        source: fn(PathBuf) -> AgentSource,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // an absent directory is normal
        };
        let Ok(root) = std::fs::canonicalize(dir) else {
            return;
        };
        let boundary = match boundary {
            Some(path) => std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            None => root.clone(),
        };
        if !root.starts_with(&boundary) {
            self.diagnostics.push(Diagnostic {
                path: dir.to_path_buf(),
                message: format!(
                    "links outside {}: the directory is not read",
                    boundary.display()
                ),
            });
            return;
        }
        let mut files: Vec<PathBuf> = entries
            .take(MAX_SCANNED_ENTRIES)
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|e| e == "md")
                    && std::fs::metadata(path).is_ok_and(|m| m.is_file())
            })
            .collect();
        files.sort();
        if files.len() > MAX_FILES {
            files.truncate(MAX_FILES);
            self.diagnostics.push(Diagnostic {
                path: dir.to_path_buf(),
                message: format!("more than {MAX_FILES} agent files: the rest are not read"),
            });
        }
        for path in files {
            let target = match std::fs::canonicalize(&path) {
                Ok(target) if target.starts_with(&boundary) => target,
                Ok(_) => {
                    self.diagnostics.push(Diagnostic {
                        path,
                        message: format!(
                            "links outside {}: the file is not read",
                            boundary.display()
                        ),
                    });
                    continue;
                }
                Err(e) => {
                    self.diagnostics.push(Diagnostic {
                        path,
                        message: format!("cannot be resolved: {e}"),
                    });
                    continue;
                }
            };
            match read_file(&path, &target, source(dir.to_path_buf())) {
                Ok(definition) => self.add(&path, definition),
                Err(message) => self.diagnostics.push(Diagnostic { path, message }),
            }
        }
    }

    fn add(&mut self, path: &Path, mut definition: AgentDefinition) {
        if definition.id.is_reserved() {
            self.diagnostics.push(Diagnostic {
                path: path.to_path_buf(),
                message: format!(
                    "`{}` is a built-in agent and cannot be replaced by a file",
                    definition.id
                ),
            });
            return;
        }
        for key in ["permission", "permissions"] {
            if definition.extra.contains_key(key) {
                self.diagnostics.push(Diagnostic {
                    path: path.to_path_buf(),
                    message: format!("`{key}` settings are not supported and have no effect"),
                });
            }
        }
        definition.source = match definition.source {
            AgentSource::OpenCode(_) => AgentSource::OpenCode(path.to_path_buf()),
            AgentSource::User(_) => AgentSource::User(path.to_path_buf()),
            AgentSource::Project(_) => AgentSource::Project(path.to_path_buf()),
            AgentSource::Builtin => AgentSource::Builtin,
        };
        self.by_id
            .insert(definition.id.clone(), Arc::new(definition));
    }
}

fn builtin_definitions() -> Vec<AgentDefinition> {
    let read_only_tools: BTreeMap<String, bool> = ["write", "edit", "bash"]
        .into_iter()
        .map(|name| (name.to_string(), false))
        .collect();
    let make = |id: &str,
                description: &str,
                mode: AgentMode,
                prompt: &str,
                tools: BTreeMap<String, bool>,
                access: AgentAccess| AgentDefinition {
        id: AgentId(id.to_string()),
        description: description.to_string(),
        mode,
        prompt: prompt.trim().to_string(),
        model: None,
        tools,
        access,
        source: AgentSource::Builtin,
        extra: BTreeMap::new(),
    };
    vec![
        make(
            BUILD,
            "Implements the task, verifies it and reports",
            AgentMode::Primary,
            // The system contract every request starts with already is the
            // build stance (implement, verify, report): `build` adds no role
            // text, so a session on the default agent sends the same
            // requests it always did.
            "",
            BTreeMap::new(),
            AgentAccess::Full,
        ),
        make(
            PLAN,
            "Researches, writes markdown plans and delegates; never changes code",
            AgentMode::Primary,
            include_str!("builtin/plan.md"),
            BTreeMap::new(),
            AgentAccess::Full,
        ),
        make(
            "explore",
            "Read-only codebase exploration",
            AgentMode::Subagent,
            include_str!("builtin/explore.md"),
            read_only_tools.clone(),
            AgentAccess::ReadOnly,
        ),
        make(
            "review",
            "Read-only code review",
            AgentMode::Subagent,
            include_str!("builtin/review.md"),
            read_only_tools,
            AgentAccess::ReadOnly,
        ),
    ]
}

#[derive(Deserialize)]
struct Header {
    description: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    tools: Option<BTreeMap<String, bool>>,
    access: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// Reads and parses one agent file; the id is the name of `path` without
/// `.md`, and the content is read from `target`, where `path` resolves.
fn read_file(path: &Path, target: &Path, source: AgentSource) -> Result<AgentDefinition, String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("the file name is not valid text")?;
    let id = AgentId::parse(stem)?;
    let file = std::fs::File::open(target).map_err(|e| format!("cannot be read: {e}"))?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot be read as text: {e}"))?;
    if text.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {} KiB", MAX_FILE_BYTES / 1024));
    }
    parse(id, &text, source)
}

fn parse(id: AgentId, text: &str, source: AgentSource) -> Result<AgentDefinition, String> {
    let (yaml, body) = frontmatter::split(text)?;
    let header: Header =
        serde_saphyr::from_str(yaml).map_err(|e| format!("invalid YAML header: {e}"))?;
    let mode = header
        .mode
        .as_deref()
        .map_or(Ok(AgentMode::All), |m| AgentMode::parse(m.trim()))?;
    let access = header
        .access
        .as_deref()
        .map_or(Ok(AgentAccess::Full), |a| AgentAccess::parse(a.trim()))?;
    Ok(AgentDefinition {
        id,
        description: header.description.unwrap_or_default().trim().to_string(),
        mode,
        prompt: body.trim().to_string(),
        model: header
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty()),
        tools: header.tools.unwrap_or_default(),
        access,
        source,
        extra: header.extra,
    })
}

#[cfg(test)]
mod tests;
