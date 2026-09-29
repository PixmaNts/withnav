//! Persistent conversation history (navigator): one SQLite file per user,
//! shared by every server process.
//!
//! SQLite is blocking: ONE dedicated writer thread owns a connection and is
//! fed by an `std::sync::mpsc` channel; read methods open their own
//! connection in `tokio::task::spawn_blocking`. WAL mode. Persistence
//! errors are logged, never fatal: a failed write must never break a live
//! conversation.
//!
//! Schema:
//! - `sessions(id TEXT PRIMARY KEY, workspace TEXT, model TEXT, profile TEXT,
//!   title TEXT, created_at TEXT, updated_at TEXT, last_seq INTEGER, transcript TEXT)`
//! - `events(session_id TEXT, seq INTEGER, kind TEXT, envelope TEXT,
//!   PRIMARY KEY(session_id, seq))`
//!
//! Stored events: every durable envelope in order, except `session.end` (a
//! resumed stream must not look closed). `llm.delta` is live-only and never
//! stored. `transcript` is the JSON of the actor's `Vec<ChatMessage>`,
//! snapshotted after every turn end.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::provider::openai::ChatMessage;

/// The stored session row's client-facing projection (the HTTP list shape,
/// minus `live` which only the registry knows).
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: Ulid,
    pub workspace: String,
    pub model: String,
    pub profile: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_seq: u64,
}

/// A full session load: the row's metadata, its stored envelopes (in seq
/// order, `session.end` excluded), and the last transcript snapshot.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub meta: StoredSession,
    pub envelopes: Vec<EventEnvelope>,
    pub transcript: Vec<ChatMessage>,
}

/// What the writer thread accepts. Every variant is fire-and-forget;
/// failures are logged on the writer thread.
enum Write {
    RecordSession {
        id: Ulid,
        workspace: String,
        model: String,
        profile: Option<String>,
    },
    AppendEvent {
        id: Ulid,
        envelope: EventEnvelope,
    },
    SaveTranscript {
        id: Ulid,
        transcript: Vec<ChatMessage>,
    },
    SetTitle {
        id: Ulid,
        title: String,
    },
    Delete {
        id: Ulid,
    },
}

/// The handle side of the store: clones a sender into the single writer
/// thread plus the shared path for blocking reads. Cheap to clone.
#[derive(Clone)]
pub struct HistoryStore {
    tx: mpsc::Sender<Write>,
    path: Arc<PathBuf>,
}

impl std::fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl HistoryStore {
    /// Opens (creating parents) and migrates the database, then spawns the
    /// single writer thread. Fails only when the database cannot be opened
    /// at all — the caller treats that as "no history".
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        migrate(&path)?;
        let (tx, rx) = mpsc::channel::<Write>();
        let writer_path = path.clone();
        std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || writer_loop(writer_path, rx))
            .map_err(|e| format!("history writer thread: {e}"))?;
        Ok(Self {
            tx,
            path: Arc::new(path),
        })
    }

    /// Registers a session row (INSERT OR IGNORE: a resumed id already has
    /// its row).
    pub fn record_session(
        &self,
        id: Ulid,
        workspace: &str,
        model: &str,
        profile: Option<&str>,
    ) {
        let _ = self.tx.send(Write::RecordSession {
            id,
            workspace: workspace.to_string(),
            model: model.to_string(),
            profile: profile.map(str::to_string),
        });
    }

    /// Appends one durable envelope (the caller filters `llm.delta` and
    /// `session.end`).
    pub fn append_event(&self, id: Ulid, envelope: EventEnvelope) {
        let _ = self.tx.send(Write::AppendEvent { id, envelope });
    }

    /// Snapshots the actor transcript after a turn end (also bumps
    /// `updated_at`; `last_seq` is owned by `append_event`).
    pub fn save_transcript(&self, id: Ulid, transcript: Vec<ChatMessage>) {
        let _ = self.tx.send(Write::SaveTranscript { id, transcript });
    }

    /// Sets the session title (first driver message; only when unset).
    pub fn set_title(&self, id: Ulid, title: String) {
        let _ = self.tx.send(Write::SetTitle { id, title });
    }

    /// Removes a session row and its events.
    pub fn delete(&self, id: Ulid) {
        let _ = self.tx.send(Write::Delete { id });
    }

    /// Sessions of one workspace with their driver message counts, newest
    /// `updated_at` first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<ListedSession>, String> {
        let path = Arc::clone(&self.path);
        let workspace = workspace.to_string();
        tokio::task::spawn_blocking(move || list_sync(&path, &workspace))
            .await
            .map_err(|e| format!("join: {e}"))?
    }

    /// Full load for resume: metadata, envelopes, transcript.
    pub async fn load(&self, id: Ulid) -> Result<Option<LoadedSession>, String> {
        let path = Arc::clone(&self.path);
        tokio::task::spawn_blocking(move || load_sync(&path, id))
            .await
            .map_err(|e| format!("join: {e}"))?
    }

    /// Whether the store knows this id (cheap EXISTS probe).
    pub async fn exists(&self, id: Ulid) -> Result<bool, String> {
        let path = Arc::clone(&self.path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(path.as_ref()).map_err(|e| format!("open: {e}"))?;
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                params![id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|e| format!("exists: {e}"))
        })
        .await
        .map_err(|e| format!("join: {e}"))?
    }
}

/// One row of the list projection: the session metadata plus its driver
/// message count (`message.received` events).
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub meta: StoredSession,
    pub message_count: u64,
}

/// The single writer thread: one connection, one command at a time.
fn writer_loop(path: PathBuf, rx: mpsc::Receiver<Write>) {
    let Ok(conn) = Connection::open(&path) else {
        tracing::error!("history writer: cannot open {}", path.display());
        // Drain so senders never block (the channel is unbounded anyway,
        // but staying here until disconnect keeps the semantics simple).
        for _ in rx {}
        return;
    };
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    for write in rx {
        if let Err(err) = apply(&conn, write) {
            tracing::error!("history write failed: {err}");
        }
    }
}

fn apply(conn: &Connection, write: Write) -> Result<(), rusqlite::Error> {
    match write {
        Write::RecordSession {
            id,
            workspace,
            model,
            profile,
        } => {
            let now = now_rfc3339();
            conn.execute(
                "INSERT OR IGNORE INTO sessions
                 (id, workspace, model, profile, title, created_at, updated_at, last_seq, transcript)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, -1, NULL)",
                params![id.to_string(), workspace, model, profile, now],
            )?;
            Ok(())
        }
        Write::AppendEvent { id, envelope } => {
            let payload = serde_json::to_string(&envelope)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.execute(
                "INSERT OR IGNORE INTO events (session_id, seq, kind, envelope)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id.to_string(), envelope.seq as i64, envelope.kind, payload],
            )?;
            conn.execute(
                "UPDATE sessions SET updated_at = ?2, last_seq = MAX(last_seq, ?3)
                 WHERE id = ?1",
                params![id.to_string(), now_rfc3339(), envelope.seq as i64],
            )?;
            Ok(())
        }
        Write::SaveTranscript { id, transcript } => {
            let payload = serde_json::to_string(&transcript)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.execute(
                "UPDATE sessions SET transcript = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), payload, now_rfc3339()],
            )?;
            Ok(())
        }
        Write::SetTitle { id, title } => {
            conn.execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1 AND title IS NULL",
                params![id.to_string(), title],
            )?;
            Ok(())
        }
        Write::Delete { id } => {
            conn.execute("DELETE FROM events WHERE session_id = ?1", params![id.to_string()])?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id.to_string()])?;
            Ok(())
        }
    }
}

/// Creates the schema when missing. Idempotent.
fn migrate(path: &Path) -> Result<(), String> {
    let conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            workspace TEXT NOT NULL,
            model TEXT NOT NULL,
            profile TEXT,
            title TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_seq INTEGER NOT NULL DEFAULT -1,
            transcript TEXT
        );
        CREATE TABLE IF NOT EXISTS events (
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL,
            envelope TEXT NOT NULL,
            PRIMARY KEY (session_id, seq)
        );",
    )
    .map_err(|e| format!("migrate: {e}"))?;
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let id: String = row.get(0)?;
    let profile: Option<String> = row.get(3)?;
    Ok(StoredSession {
        id: Ulid::from_string(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        workspace: row.get(1)?,
        model: row.get(2)?,
        profile,
        title: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        last_seq: row.get::<_, i64>(7)?.max(0) as u64,
    })
}

fn list_sync(path: &Path, workspace: &str) -> Result<Vec<ListedSession>, String> {
    let conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.workspace, s.model, s.profile, s.title,
                    s.created_at, s.updated_at, s.last_seq,
                    (SELECT COUNT(*) FROM events e
                     WHERE e.session_id = s.id AND e.kind = 'message.received')
             FROM sessions s WHERE s.workspace = ?1
             ORDER BY s.updated_at DESC, s.id DESC",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![workspace], |row| {
            Ok(ListedSession {
                meta: row_to_session(row)?,
                message_count: row.get::<_, i64>(8)?.max(0) as u64,
            })
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(session) => out.push(session),
            Err(err) => tracing::error!("history list: bad row skipped: {err}"),
        }
    }
    Ok(out)
}

fn load_sync(path: &Path, id: Ulid) -> Result<Option<LoadedSession>, String> {
    let conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let Some(meta) = conn
        .query_row(
            "SELECT id, workspace, model, profile, title, created_at, updated_at, last_seq,
                    transcript
             FROM sessions WHERE id = ?1",
            params![id.to_string()],
            |row| {
                let meta = row_to_session(row)?;
                let transcript: Option<String> = row.get(8)?;
                Ok((meta, transcript))
            },
        )
        .optional()
        .map_err(|e| format!("load row: {e}"))?
    else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare("SELECT envelope FROM events WHERE session_id = ?1 ORDER BY seq ASC")
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query: {e}"))?;
    let mut envelopes = Vec::new();
    for row in rows {
        let raw = row.map_err(|e| format!("row: {e}"))?;
        match serde_json::from_str::<EventEnvelope>(&raw) {
            Ok(envelope) => envelopes.push(envelope),
            Err(err) => tracing::error!("history load: bad envelope skipped: {err}"),
        }
    }
    let transcript = meta
        .1
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<ChatMessage>>(raw).ok())
        .unwrap_or_default();
    Ok(Some(LoadedSession {
        meta: meta.0,
        envelopes,
        transcript,
    }))
}

/// Current UTC time as RFC 3339 with millisecond precision — same shape as
/// the event timestamps, so ORDER BY updated_at sorts chronologically.
fn now_rfc3339() -> String {
    crate::bus::event::now_rfc3339()
}

// --- the persisting emitter ---

use crate::bus::sequencer::Emitter;
use crate::bus::SubscribeReply;
use tokio::sync::oneshot;

/// Wraps a session's [`SessionBus`] as the sequencer's emitter and forwards
/// every DURABLE envelope — except `llm.delta` (live-only, never stored) and
/// `session.end` (a resumed stream must not look closed) — to the writer
/// thread, in seq order, synchronously inside the single sequencer task.
pub struct PersistingEmitter {
    bus: crate::bus::SessionBus,
    session_id: Ulid,
    store: HistoryStore,
}

impl PersistingEmitter {
    pub fn new(
        session_id: Ulid,
        bus: crate::bus::SessionBus,
        store: HistoryStore,
    ) -> Self {
        Self {
            bus,
            session_id,
            store,
        }
    }
}

impl Emitter for PersistingEmitter {
    fn emit(&mut self, envelope: EventEnvelope) {
        let kind = envelope.kind.as_str();
        let durable = kind != "llm.delta";
        let is_end = kind == "session.end";
        if durable && !is_end {
            // The first driver message becomes the title (the store guards:
            // only when the title is still NULL).
            if kind == "message.received" {
                if let Some(content) = envelope.payload["content"].as_str() {
                    self.store
                        .set_title(self.session_id, title_from_message(content));
                }
            }
            self.store.append_event(self.session_id, envelope.clone());
        }
        self.bus.emit(envelope);
    }

    fn subscribe_since(
        &mut self,
        cursor: Option<u64>,
        next_seq: u64,
        reply: oneshot::Sender<SubscribeReply>,
    ) {
        self.bus.subscribe_since(cursor, next_seq, reply);
    }
}

/// Derives the session title from the first driver message: any trailing
/// `<editor-context>…</editor-context>` block removed, whitespace collapsed,
/// cut at 60 characters.
pub fn title_from_message(content: &str) -> String {
    let stripped = strip_editor_context(content);
    let collapsed: String = stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(60).collect()
}

/// Removes a TRAILING `<editor-context>…</editor-context>` block (the editor
/// appends it after the driver's text). Non-trailing blocks are kept.
fn strip_editor_context(content: &str) -> String {
    const OPEN: &str = "<editor-context>";
    const CLOSE: &str = "</editor-context>";
    let Some(close_pos) = content.rfind(CLOSE) else {
        return content.to_string();
    };
    // The block must be trailing (only whitespace after the close tag).
    if !content[close_pos + CLOSE.len()..].trim().is_empty() {
        return content.to_string();
    }
    let before_close = &content[..close_pos];
    let Some(open_pos) = before_close.rfind(OPEN) else {
        return content.to_string();
    };
    format!(
        "{}{}",
        &content[..open_pos],
        &content[close_pos + CLOSE.len()..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::event::EventKind;

    fn envelope(seq: u64, kind: EventKind) -> EventEnvelope {
        EventEnvelope::new(Ulid::new(), seq, kind, serde_json::json!({ "n": seq }))
    }

    fn temp_store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::open(dir.path().join("sessions.db")).expect("open");
        (dir, store)
    }

    // Given a fresh store
    // When a session is recorded, events appended, transcript saved
    // Then list/load return everything and list is newest-first
    #[tokio::test]
    async fn record_append_snapshot_roundtrip() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        let id2 = Ulid::new();
        store.record_session(id, "/w", "cognition", Some("navigator"));
        store.record_session(id2, "/w", "default", None);
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.append_event(id, envelope(1, EventKind::MessageReceived));
        store.save_transcript(
            id,
            vec![ChatMessage::User {
                content: "hi".into(),
            }],
        );
        // id2 updated later: it must list first even though id was created first.
        store.append_event(id2, envelope(0, EventKind::SessionStart));
        // Let the writer drain.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let listed = store.list("/w").await.expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].meta.id, id2, "newest updated_at first");

        let loaded = store.load(id).await.expect("load").expect("present");
        assert_eq!(loaded.meta.model, "cognition");
        assert_eq!(loaded.meta.profile.as_deref(), Some("navigator"));
        assert_eq!(loaded.envelopes.len(), 2);
        assert_eq!(loaded.envelopes[0].seq, 0);
        assert_eq!(loaded.envelopes[1].seq, 1);
        assert_eq!(loaded.transcript.len(), 1);
        assert_eq!(loaded.meta.last_seq, 1);
        assert!(store.exists(id2).await.unwrap());
    }

    // Given a store with a session
    // When set_title runs twice
    // Then only the FIRST title sticks (title IS NULL guard)
    #[tokio::test]
    async fn title_is_set_once() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "m", None);
        store.set_title(id, "first".into());
        store.set_title(id, "second".into());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let loaded = store.load(id).await.expect("load").expect("present");
        assert_eq!(loaded.meta.title.as_deref(), Some("first"));
    }

    // Given a store with a session
    // When delete runs
    // Then list and load no longer see it
    #[tokio::test]
    async fn delete_removes_row_and_events() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "m", None);
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.delete(id);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(store.load(id).await.expect("load").is_none());
        assert!(store.list("/w").await.expect("list").is_empty());
    }

    // Given a store
    // When list runs for another workspace
    // Then it is empty (per-workspace scoping)
    #[tokio::test]
    async fn list_is_per_workspace() {
        let (_dir, store) = temp_store();
        store.record_session(Ulid::new(), "/a", "m", None);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(store.list("/b").await.expect("list").is_empty());
        assert_eq!(store.list("/a").await.expect("list").len(), 1);
    }

    // Given a store file
    // When open runs twice on the same path
    // Then the schema migration is idempotent
    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.db");
        let _a = HistoryStore::open(&path).expect("first open");
        let b = HistoryStore::open(&path).expect("second open");
        b.record_session(Ulid::new(), "/w", "m", None);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(b.list("/w").await.expect("list").len(), 1);
    }

    // ---- title derivation ----

    // Given a driver message with a trailing editor-context block
    // When the title is derived
    // Then the block is gone, whitespace collapses, cut at 60 chars
    #[test]
    fn title_strips_trailing_editor_context_and_collapses() {
        let msg = "fix  the\n  bug   here<editor-context>{\"file\":\"a.rs\"}</editor-context>";
        assert_eq!(title_from_message(msg), "fix the bug here");
        // Plain message, untouched.
        assert_eq!(title_from_message("  simple   ask "), "simple ask");
        // 60-char cut.
        let long = "x".repeat(80);
        assert_eq!(title_from_message(&long).chars().count(), 60);
        // Non-trailing block is KEPT (only a trailing block is editor-added).
        let mid = "see <editor-context>x</editor-context> then report";
        assert_eq!(title_from_message(mid), "see <editor-context>x</editor-context> then report");
        // Unclosed block: kept verbatim.
        let unclosed = "hello <editor-context> junk";
        assert_eq!(title_from_message(unclosed), "hello <editor-context> junk");
    }
}
