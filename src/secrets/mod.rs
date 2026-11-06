//! Secrets the Captain hands to the tools (an API key, a token): kept by the session service,
//! given to the processes the agent starts as environment variables, and replaced by
//! `[secret NAME]` in any text on its way to the model, the events or the history.
//!
//! The value reaches the server only through `POST /v1/sessions/{id}/secrets/{request_id}`
//! (`server/routes.rs`, `session/secrets.rs`); the model reads metadata (`stored as $NAME …`).
//!
//! Where a value lives:
//! * scope `session`: in this process's memory, for the conversation tree that asked (a root
//!   and its subagents), gone when it expires, is revoked, or the process ends; availability
//!   `session` also ends it when that conversation stops;
//! * scope `workspace` with a timed availability: also in `<data dir>/credentials/secrets.json`
//!   (mode 0600, directory 0700), in plain text, with its expiry, so other conversations of the
//!   workspace and a restarted server find it. Expiry and revocation rewrite the file; the last
//!   secret gone removes it. The `credentials` component puts the file under the fixed floor's
//!   credential names, so a tool touching it asks the Captain.
//!
//! Redaction is literal: exact occurrences of a value of 8 or more characters. A value the
//! agent transforms (encodes, splits, reverses) is not recognised: see `book/src/secrets.md`.

mod file;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ulid::Ulid;
pub use withnav_client_api::secrets::{
    SECRET_KIND, SecretAccepted, SecretNote, SecretOutcome, SecretRequestDetails, SecretScope,
    SecretSubmission, SecretTtl, SecretView, SecretsBody, valid_secret_name,
};

/// The tool an agent calls to ask the Captain for a secret.
pub const REQUEST_SECRET_TOOL: &str = "request_secret";

/// Values shorter than this are never redacted: a short value would replace ordinary words.
pub const MIN_REDACTED_CHARS: usize = 8;

/// The largest value accepted, in bytes.
pub const MAX_VALUE_BYTES: usize = 16 * 1024;

/// Names a secret cannot take: they change how a shell or a program starts, so a value set
/// under one would run code or hide commands rather than authenticate anything.
const RESERVED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "PWD",
    "OLDPWD",
    "TMPDIR",
    "TERM",
    "LANG",
    "IFS",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "CDPATH",
    "GLOBIGNORE",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "RUBYLIB",
    "GIT_SSH_COMMAND",
    "GIT_EXEC_PATH",
    "EDITOR",
    "VISUAL",
    "PAGER",
];

/// Prefixes a secret name cannot start with, for the same reason.
const RESERVED_PREFIXES: &[&str] = &["LD_", "DYLD_", "BASH_", "LC_", "WITHNAV_", "NAVAI_"];

/// Whether `name` can name a secret: the environment variable rule, and not reserved.
pub fn check_name(name: &str) -> Result<(), SecretError> {
    if !valid_secret_name(name) {
        return Err(SecretError::InvalidName(name.to_owned()));
    }
    if RESERVED_NAMES.contains(&name) || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(SecretError::ReservedName(name.to_owned()));
    }
    Ok(())
}

/// One stored secret.
#[derive(Clone)]
struct Entry {
    name: String,
    value: String,
    scope: SecretScope,
    ttl: SecretTtl,
    /// The conversation tree that stored it (its root session).
    owner: Ulid,
    workspace: PathBuf,
    created_at: i64,
    expires_at: Option<i64>,
    use_count: u64,
}

impl Entry {
    /// Persisted: a workspace secret that outlives its conversation.
    fn persisted(&self) -> bool {
        self.scope == SecretScope::Workspace && self.expires_at.is_some()
    }

    fn visible_to(&self, owner: Ulid, workspace: &Path) -> bool {
        match self.scope {
            SecretScope::Session => self.owner == owner,
            SecretScope::Workspace => self.workspace == workspace,
        }
    }

    fn view(&self) -> SecretView {
        let chars: Vec<char> = self.value.chars().collect();
        let last4 = if chars.len() >= MIN_REDACTED_CHARS {
            chars[chars.len() - 4..].iter().collect()
        } else {
            String::new()
        };
        SecretView {
            name: self.name.clone(),
            scope: self.scope,
            ttl: self.ttl,
            expires_at: self.expires_at.map(rfc3339),
            created_at: rfc3339(self.created_at),
            use_count: self.use_count,
            last4,
        }
    }
}

/// Why a secret was not stored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("invalid secret name {0:?}: expected ^[A-Z][A-Z0-9_]{{1,63}}$")]
    InvalidName(String),
    #[error("{0} is reserved: it changes how processes start, it cannot name a secret")]
    ReservedName(String),
    #[error("the secret value is empty")]
    EmptyValue,
    #[error("the secret value is larger than {MAX_VALUE_BYTES} bytes")]
    TooLarge,
    #[error("the secret value contains a NUL character")]
    Nul,
}

/// The service's secrets: every conversation's, plus the persisted workspace ones.
pub struct SecretStore {
    file: Option<PathBuf>,
    entries: Mutex<Vec<Entry>>,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// A store that keeps everything in memory: workspace secrets are not saved.
    pub fn in_memory() -> Self {
        Self {
            file: None,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The store whose workspace secrets live in `<data_dir>/credentials/secrets.json`. What the
    /// file holds is read now; expired entries are dropped. An unreadable file is reported
    /// and left as it is (nothing is loaded from it, and nothing overwrites it until a
    /// workspace secret is stored).
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("credentials").join("secrets.json");
        let now = now();
        let entries = match file::load(&path) {
            Ok(saved) => saved
                .into_iter()
                .filter(|saved| saved.expires_at > now && check_name(&saved.name).is_ok())
                .map(|saved| Entry {
                    name: saved.name,
                    value: saved.value,
                    scope: SecretScope::Workspace,
                    ttl: saved.ttl,
                    owner: Ulid::nil(),
                    workspace: saved.workspace,
                    created_at: saved.created_at,
                    expires_at: Some(saved.expires_at),
                    use_count: saved.use_count,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "workspace secrets not loaded");
                Vec::new()
            }
        };
        let store = Self {
            file: Some(path),
            entries: Mutex::new(entries),
        };
        store.save(&store.lock());
        store
    }

    /// Where workspace secrets are saved, if anywhere.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drops what expired; true when a persisted entry went.
    fn purge(entries: &mut Vec<Entry>, now: i64) -> bool {
        let mut persisted = false;
        entries.retain(|entry| {
            let alive = entry.expires_at.is_none_or(|at| at > now);
            persisted |= !alive && entry.persisted();
            alive
        });
        persisted
    }

    fn save(&self, entries: &[Entry]) {
        let Some(path) = &self.file else {
            return;
        };
        let saved: Vec<file::Saved> = entries
            .iter()
            .filter(|entry| entry.persisted())
            .map(|entry| file::Saved {
                name: entry.name.clone(),
                value: entry.value.clone(),
                workspace: entry.workspace.clone(),
                ttl: entry.ttl,
                created_at: entry.created_at,
                expires_at: entry.expires_at.unwrap_or_default(),
                use_count: entry.use_count,
            })
            .collect();
        if let Err(error) = file::save(path, &saved) {
            tracing::warn!(path = %path.display(), %error, "workspace secrets not saved");
        }
    }

    /// Stores `value` as `name` for the conversation tree `owner` in `workspace`, replacing the
    /// secret of that name and scope it could see.
    pub fn put(
        &self,
        owner: Ulid,
        workspace: &Path,
        name: &str,
        value: &str,
        ttl: SecretTtl,
        scope: SecretScope,
    ) -> Result<SecretView, SecretError> {
        check_name(name)?;
        if value.is_empty() {
            return Err(SecretError::EmptyValue);
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(SecretError::TooLarge);
        }
        if value.contains('\0') {
            return Err(SecretError::Nul);
        }
        let workspace = canonical(workspace);
        let now = now();
        let entry = Entry {
            name: name.to_owned(),
            value: value.to_owned(),
            scope,
            ttl,
            owner,
            workspace: workspace.clone(),
            created_at: now,
            expires_at: ttl.seconds().map(|seconds| now.saturating_add(seconds)),
            use_count: 0,
        };
        let view = entry.view();
        let mut entries = self.lock();
        Self::purge(&mut entries, now);
        // Replaces the secret of that name in the same scope; a session secret then hides a
        // workspace one of the same name for this conversation only.
        entries
            .retain(|e| !(e.name == name && e.scope == scope && e.visible_to(owner, &workspace)));
        entries.push(entry);
        self.save(&entries);
        Ok(view)
    }

    /// Removes the secret `name` the tree `owner` sees in `workspace`; false when none.
    pub fn revoke(&self, owner: Ulid, workspace: &Path, name: &str) -> bool {
        let workspace = canonical(workspace);
        let mut entries = self.lock();
        let purged = Self::purge(&mut entries, now());
        let before = entries.len();
        entries.retain(|e| !(e.name == name && e.visible_to(owner, &workspace)));
        let removed = entries.len() != before;
        if removed || purged {
            self.save(&entries);
        }
        removed
    }

    /// Forgets the secrets that last as long as the conversation tree `owner`.
    pub fn end_session(&self, owner: Ulid) {
        let mut entries = self.lock();
        entries.retain(|e| !(e.owner == owner && e.ttl == SecretTtl::Session));
    }

    /// What `owner` sees in `workspace`, by name; a session secret hides a workspace one of
    /// the same name.
    fn visible<'a>(entries: &'a mut [Entry], owner: Ulid, workspace: &Path) -> Vec<&'a mut Entry> {
        let mut visible: Vec<&mut Entry> = entries
            .iter_mut()
            .filter(|e| e.visible_to(owner, workspace))
            .collect();
        visible.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then((a.scope == SecretScope::Workspace).cmp(&(b.scope == SecretScope::Workspace)))
        });
        visible.dedup_by(|later, first| later.name == first.name);
        visible
    }

    /// The secrets `owner` sees in `workspace`, metadata only.
    pub fn list(&self, owner: Ulid, workspace: &Path) -> Vec<SecretView> {
        let workspace = canonical(workspace);
        let mut entries = self.lock();
        if Self::purge(&mut entries, now()) {
            self.save(&entries);
        }
        Self::visible(&mut entries, owner, &workspace)
            .into_iter()
            .map(|e| e.view())
            .collect()
    }

    /// The environment a process started for `owner` in `workspace` receives: one
    /// `(NAME, value)` per visible secret, each counted as one more use.
    pub fn environment(&self, owner: Ulid, workspace: &Path) -> Vec<(String, String)> {
        let workspace = canonical(workspace);
        let mut entries = self.lock();
        let purged = Self::purge(&mut entries, now());
        let mut persisted = purged;
        let env = Self::visible(&mut entries, owner, &workspace)
            .into_iter()
            .map(|e| {
                e.use_count = e.use_count.saturating_add(1);
                persisted |= e.persisted();
                (e.name.clone(), e.value.clone())
            })
            .collect();
        if persisted {
            self.save(&entries);
        }
        env
    }

    /// `text` with every value `owner` sees (8 characters or more) replaced by
    /// `[secret NAME]`; borrowed when nothing matched.
    pub fn redact<'t>(&self, owner: Ulid, workspace: &Path, text: &'t str) -> Cow<'t, str> {
        let workspace = canonical(workspace);
        let mut entries = self.lock();
        Self::purge(&mut entries, now());
        let mut values: Vec<(String, String)> = entries
            .iter()
            .filter(|e| {
                e.visible_to(owner, &workspace) && e.value.chars().count() >= MIN_REDACTED_CHARS
            })
            .map(|e| (e.value.clone(), e.name.clone()))
            .collect();
        drop(entries);
        redact_values(text, &mut values)
    }
}

/// Replaces each value (longest first, so one containing another wins) with `[secret NAME]`.
fn redact_values<'t>(text: &'t str, values: &mut [(String, String)]) -> Cow<'t, str> {
    if values
        .iter()
        .all(|(value, _)| !text.contains(value.as_str()))
    {
        return Cow::Borrowed(text);
    }
    values.sort_by_key(|v| std::cmp::Reverse(v.0.len()));
    let mut out = text.to_owned();
    for (value, name) in values.iter() {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("[secret {name}]"));
        }
    }
    Cow::Owned(out)
}

/// One conversation tree's view of the store: what its tools receive and what is hidden
/// from its model.
#[derive(Clone)]
pub struct SecretAccess {
    store: Arc<SecretStore>,
    owner: Ulid,
    workspace: PathBuf,
}

impl std::fmt::Debug for SecretAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretAccess")
            .field("owner", &self.owner)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl SecretAccess {
    pub fn new(store: Arc<SecretStore>, owner: Ulid, workspace: &Path) -> Self {
        Self {
            store,
            owner,
            workspace: canonical(workspace),
        }
    }

    /// See [`SecretStore::environment`].
    pub fn environment(&self) -> Vec<(String, String)> {
        self.store.environment(self.owner, &self.workspace)
    }

    /// See [`SecretStore::redact`].
    pub fn redact<'t>(&self, text: &'t str) -> Cow<'t, str> {
        self.store.redact(self.owner, &self.workspace, text)
    }

    /// Redacts `text` in place.
    pub fn redact_in_place(&self, text: &mut String) {
        if let Cow::Owned(redacted) = self.redact(text) {
            *text = redacted;
        }
    }

    /// Redacts every string of a JSON value in place (tool metadata, arguments).
    pub fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => self.redact_in_place(text),
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| self.redact_json(v)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|v| self.redact_json(v)),
            _ => {}
        }
    }

    /// Redacts the text of a transcript message: content, reasoning and tool arguments.
    pub fn redact_message(&self, message: &mut crate::provider::openai::ChatMessage) {
        use crate::provider::openai::ChatMessage;
        match message {
            ChatMessage::System { content }
            | ChatMessage::User { content, .. }
            | ChatMessage::Tool { content, .. } => self.redact_in_place(content),
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                for text in [content, reasoning_content].into_iter().flatten() {
                    self.redact_in_place(text);
                }
                for call in tool_calls {
                    self.redact_in_place(&mut call.function.arguments);
                }
            }
        }
    }
}

/// The workspace key: canonical when it exists, as given otherwise.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// RFC 3339 for a unix time in seconds.
pub fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// The metadata a model reads for a stored secret, e.g.
/// `stored as $OPENAI_API_KEY (expires 2026-11-07T10:00:00Z, scope: this session)`.
pub fn stored_text(view: &SecretView) -> String {
    let expires = match &view.expires_at {
        Some(at) => format!("expires {at}"),
        None => "expires when this conversation stops".to_owned(),
    };
    let scope = match view.scope {
        SecretScope::Session => "this session",
        SecretScope::Workspace => "this workspace",
    };
    format!(
        "stored as ${} ({expires}, scope: {scope}). It is set in the environment of the commands you run; use it as ${} and never print it.",
        view.name, view.name
    )
}

/// What a model reads for a declined request.
pub fn declined_text(name: &str) -> String {
    format!("declined by the Captain: ${name} was not provided; continue without it")
}

#[cfg(test)]
mod tests;
