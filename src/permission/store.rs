//! The SQLite connection and schema shared by rules, decisions and validations.

use std::{path::Path, time::Duration};

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

/// An independent connection to the shared database. The connection sits
/// behind a lock so that the agent loop can hold the store across the awaits of
/// one decision; each call is a short local query.
pub struct PermissionStore {
    pub(crate) connection: std::sync::Mutex<Connection>,
}

impl PermissionStore {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        std::fs::create_dir_all(data_dir.as_ref())?;
        let connection = Connection::open(data_dir.as_ref().join("withnav.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(connection)
    }

    /// A private database that lives as long as the store: an actor built
    /// outside a session starts with no rule and keeps what it learns to itself.
    pub fn in_memory() -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> Result<Self, Error> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS permission_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                action TEXT NOT NULL, resource TEXT NOT NULL,
                action_glob TEXT NOT NULL, resource_glob TEXT NOT NULL,
                action_bare_glob TEXT, resource_bare_glob TEXT,
                precision INTEGER NOT NULL,
                effect TEXT NOT NULL CHECK(effect IN ('allow','ask','deny')),
                scope TEXT NOT NULL CHECK(scope IN ('global','project','agent')),
                scope_value TEXT NOT NULL,
                origin TEXT NOT NULL CHECK(origin IN ('always','manual','flow')),
                created_at TEXT NOT NULL, revoked_at TEXT
            );
            CREATE TABLE IF NOT EXISTS permission_decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                time TEXT NOT NULL, session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL, call_id TEXT NOT NULL, request_id TEXT,
                agent TEXT NOT NULL, workspace TEXT NOT NULL, tool TEXT NOT NULL,
                call_snapshot TEXT NOT NULL, command_matches TEXT NOT NULL,
                rule_id INTEGER, effect TEXT CHECK(effect IN ('allow','ask','deny')),
                flow_verdict TEXT, captain_answer TEXT,
                outcome TEXT NOT NULL CHECK(outcome IN
                    ('allowed','denied','pending','cancelled','expired')),
                approved_by TEXT
            );
            CREATE INDEX IF NOT EXISTS permission_decisions_session
                ON permission_decisions(session_id, id);
            CREATE INDEX IF NOT EXISTS permission_decisions_time
                ON permission_decisions(time DESC, id DESC);
            CREATE INDEX IF NOT EXISTS permission_decisions_session_time
                ON permission_decisions(session_id, time DESC, id DESC);",
        )?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS validations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL CHECK(kind IN ('skill','flow')),
                name TEXT NOT NULL, location TEXT NOT NULL, content_sha256 TEXT NOT NULL,
                validated_at TEXT NOT NULL, revoked_at TEXT
            );
            CREATE INDEX IF NOT EXISTS validations_current
                ON validations(kind, location, content_sha256) WHERE revoked_at IS NULL;",
        )?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
    }

    pub(crate) fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) fn path_text(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::Invalid("permission paths must be valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_connections_share_wal_database_without_seeding_rules() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let first = PermissionStore::open(&data).unwrap();
        let second = PermissionStore::open(&data).unwrap();
        let journal: String = first
            .db()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let timeout: i64 = first
            .db()
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(timeout, 5000);
        assert!(second
            .list(&super::super::rules::RuleFilter::default())
            .unwrap()
            .is_empty());
        assert!(second
            .list_decisions(&super::super::decisions::DecisionFilter::default(), 100)
            .unwrap()
            .is_empty());
    }
}
