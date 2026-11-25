//! The SQLite connection and schema of the memory register.

use std::{path::Path, time::Duration};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::projects::Project;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A request the register refuses as written: a text over its limit, an unknown name, a
    /// change that does not apply to the item's state.
    #[error("{0}")]
    Invalid(String),
    #[error("no {what} {id}")]
    NotFound { what: &'static str, id: String },
    /// The writer read a version that is no longer the current one. Nothing was written.
    #[error("the item is at version {current}")]
    VersionConflict { current: u32 },
    /// The change would let more projects read something, and the Captain has not confirmed it.
    /// Nothing was written; `affected_projects` are the projects that would newly read it, or
    /// for a project joining a group the members it would share with.
    #[error("widening requires the Captain's confirmation")]
    WideningRequiresConfirmation { affected_projects: Vec<Project> },
}

impl Error {
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(super) fn not_found(what: &'static str, id: &str) -> Self {
        Self::NotFound {
            what,
            id: id.to_owned(),
        }
    }
}

/// Who wrote, drafted or confirmed something. The host stamps it from what it knows of the
/// caller; it is never read from the text being stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Actor {
    Captain,
    Agent,
    /// WithNav itself, for what no one asked for: a session linked to its project when it starts.
    Host,
}

text_enum!(Actor, "actor", {
    Captain => "captain",
    Agent => "agent",
    Host => "host",
});

/// An independent connection to the shared database, behind a lock: each call is one short
/// local query or one immediate transaction.
pub struct MemoryStore {
    connection: std::sync::Mutex<Connection>,
}

impl MemoryStore {
    /// Opens `withnav.db` under `data_dir`, creating the register's tables when missing.
    ///
    /// # Errors
    ///
    /// When the directory cannot be created or the database cannot be opened or prepared.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        std::fs::create_dir_all(data_dir.as_ref())?;
        let connection = Connection::open(data_dir.as_ref().join("withnav.db"))?;
        // The session writer and the permission store hold their own connections to this file:
        // a write waits for theirs instead of failing with "database is locked".
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(connection)
    }

    /// A private register that lives as long as the store.
    ///
    /// # Errors
    ///
    /// When SQLite cannot prepare the schema.
    pub fn in_memory() -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> Result<Self, Error> {
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
    }

    /// Runs `work` on the blocking pool. A query can wait up to the busy timeout on another
    /// connection's write; async code calls the store through here so that the wait never holds
    /// a runtime thread.
    ///
    /// # Errors
    ///
    /// What `work` answers, or [`Error::Invalid`] when its task panicked or was cancelled.
    pub async fn run<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        work: impl FnOnce(&Self) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || work(&store))
            .await
            .map_err(|error| Error::invalid(format!("memory database task failed: {error}")))?
    }

    /// Reads through the connection. A lock poisoned by a panicking reader is taken anyway: a
    /// transaction that did not commit was rolled back, so the connection holds nothing partial.
    pub(super) fn read<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        work(&self.connection.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Runs `work` in one immediate transaction, committed only when it succeeds. Immediate
    /// takes the write lock before the first read, so that two connections cannot both read
    /// version `n` and both write `n + 1`: the second waits, then reads what the first wrote.
    pub(super) fn write<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = work(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }
}

/// Text as the register keeps it: the blank space around it removed, nothing else changed. A
/// text over `max` characters is refused, never cut: a decision stored without its last
/// sentence would read as a different decision.
pub(super) fn bounded(text: &str, what: &str, max: usize) -> Result<String, Error> {
    let text = text.trim();
    let count = text.chars().count();
    if count > max {
        return Err(Error::invalid(format!(
            "the {what} is {count} characters long; the limit is {max}"
        )));
    }
    Ok(text.to_owned())
}

/// Like [`bounded`], for a text that must say something.
pub(super) fn required(text: &str, what: &str, max: usize) -> Result<String, Error> {
    let text = bounded(text, what, max)?;
    if text.is_empty() {
        return Err(Error::invalid(format!("the {what} is empty")));
    }
    Ok(text)
}

/// `text` on one line, each run of blank space as one space.
pub(super) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Current UTC time in the shape of the event timestamps, so that text order is time order.
pub(super) fn now() -> String {
    withnav_client_api::event::now_rfc3339()
}

/// A new identifier: 20 hexadecimal characters of OS randomness. Not a ULID: the Captain names
/// an item by its first characters, and those of a ULID are its creation time, shared by
/// everything written in the same quarter of an hour.
pub(super) fn new_id() -> Result<String, Error> {
    let mut bytes = [0u8; 10];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| Error::invalid(format!("no randomness for an identifier: {error}")))?;
    Ok(hex::encode(bytes))
}

// Additive tables in the database the session store and the permission store already share;
// a build that predates them ignores them. A later column is added with
// `ALTER TABLE … ADD COLUMN` after this batch, guarded by `pragma_table_info`.
//
// `memory_versions` is the history the Captain was promised ("préservation de l'historique",
// 2026-11-25): triggers refuse to change or drop a row whatever code runs the statement.
// Removing sensitive text for good will need its own operation that lifts them on purpose.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL,
    group_answered_at TEXT
);
CREATE TABLE IF NOT EXISTS project_aliases (
    alias TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('git_common_dir','path')),
    project_id TEXT NOT NULL REFERENCES projects(id), added_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS memory_groups (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL, retired_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS memory_groups_live_name
    ON memory_groups(name COLLATE NOCASE) WHERE retired_at IS NULL;
CREATE TABLE IF NOT EXISTS project_groups (
    project_id TEXT NOT NULL, group_id TEXT NOT NULL, PRIMARY KEY(project_id, group_id)
);
CREATE TABLE IF NOT EXISTS session_projects (
    session_id TEXT NOT NULL, project_id TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY(session_id, project_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS session_projects_active
    ON session_projects(session_id) WHERE active = 1;
CREATE TABLE IF NOT EXISTS membership_changes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    relation TEXT NOT NULL CHECK(relation IN ('project_group','session_project')),
    subject TEXT NOT NULL, object TEXT NOT NULL,
    change TEXT NOT NULL CHECK(change IN ('add','remove')),
    status TEXT NOT NULL CHECK(status IN ('applied','proposed','rejected')),
    actor TEXT NOT NULL, session_id TEXT, at TEXT NOT NULL, decided_at TEXT
);
CREATE TABLE IF NOT EXISTS memory_items (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL
        CHECK(kind IN ('decision','convention','preference','environment','pitfall')),
    scope TEXT NOT NULL CHECK(scope IN ('global','group','project','session')),
    scope_ref TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK(status IN ('active','proposed','invalidated','rejected')),
    current_version INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS memory_items_scope ON memory_items(scope, scope_ref, status);
CREATE TABLE IF NOT EXISTS memory_versions (
    item_id TEXT NOT NULL REFERENCES memory_items(id), version INTEGER NOT NULL,
    change TEXT NOT NULL
        CHECK(change IN ('created','updated','invalidated','restored','rescoped')),
    title TEXT NOT NULL, body TEXT NOT NULL, why TEXT, quote TEXT,
    scope TEXT NOT NULL, scope_ref TEXT NOT NULL,
    source_actor TEXT NOT NULL, drafted_by TEXT NOT NULL, confirmed_by TEXT,
    source_session_id TEXT, source_event_seq INTEGER, reason TEXT, at TEXT NOT NULL,
    PRIMARY KEY(item_id, version)
);
CREATE TRIGGER IF NOT EXISTS memory_versions_no_update BEFORE UPDATE ON memory_versions
    BEGIN SELECT RAISE(ABORT, 'memory versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS memory_versions_no_delete BEFORE DELETE ON memory_versions
    BEGIN SELECT RAISE(ABORT, 'memory versions are append-only'); END;
";
