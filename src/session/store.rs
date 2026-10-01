//! Persistent conversation history (navigator): one SQLite file per user,
//! shared by every server process.
//!
//! SQLite is blocking: ONE dedicated writer thread owns a connection and is
//! fed by an `std::sync::mpsc` channel; read methods open their own
//! connection in `tokio::task::spawn_blocking`. WAL mode.
//!
//! Writes: every write is ONE transaction (an event insert and the
//! `last_seq` bump commit together or not at all). Enqueueing never waits
//! on SQLite, so the sequencer stays off the disk. A failure is never only a
//! log line: writes whose caller needs to know ([`HistoryStore::record_session`],
//! [`HistoryStore::delete`]) are acknowledged through a oneshot; every other
//! write (event appends, transcript snapshots, titles) is fire-and-forget and
//! its failure is remembered by the writer until [`HistoryStore::flush`],
//! which is also a barrier: it returns once every earlier write has been
//! applied, carrying the first error since the previous flush. A failed write
//! still never breaks a live conversation: the callers decide what a failure
//! means.
//!
//! Two notions that must not be confused. "Conversation resumable" means the
//! row and its events are in this store: `POST /v1/sessions` with `resume`
//! rebuilds an actor and sequencer under the same id, whether or not the
//! session is still in memory. "Runtime stopped" means the in-memory actor
//! and sequencer are gone; that says nothing about the store, which keeps the
//! conversation until `DELETE` removes it. The store knows only the former;
//! the registry in `session::service` owns the latter. A stopped runtime's
//! `session.end` is stored like any other durable event, so a stored
//! conversation does not by itself say whether a runtime is live.
//!
//! Schema:
//! - `sessions(id TEXT PRIMARY KEY, workspace TEXT, model TEXT, profile TEXT,
//!   title TEXT, created_at TEXT, updated_at TEXT, last_seq INTEGER, transcript TEXT)`
//! - `events(session_id TEXT, seq INTEGER, kind TEXT, envelope TEXT,
//!   PRIMARY KEY(session_id, seq))`
//!
//! Stored events: every durable envelope in order, `session.end` included.
//! Recording it keeps `last_seq` at the highest seq any client has seen, so a
//! resumed session continues after it and never reuses a seq; the resume path
//! (not the store) keeps it from replaying to subscribers, so the resumed
//! stream does not look closed. `llm.delta` is live-only and never stored.
//! `transcript` is the JSON of the actor's `Vec<ChatMessage>`, snapshotted
//! after every turn end.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::oneshot;
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
/// order, any `session.end` included), and the last transcript snapshot.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub meta: StoredSession,
    pub envelopes: Vec<EventEnvelope>,
    pub transcript: Vec<ChatMessage>,
}

/// The result channel of an acknowledged write.
type Ack = oneshot::Sender<Result<(), String>>;

/// What the writer thread accepts.
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

/// One message on the writer channel.
enum Msg {
    /// A write; `ack` (when present) receives its outcome, otherwise a
    /// failure is remembered for every later [`Msg::Flush`].
    Write(Box<Write>, Option<Ack>),
    /// Barrier: replies with the first error any write has hit since the
    /// store was opened.
    Flush(Ack),
}

/// Why a single write failed.
#[derive(Debug, thiserror::Error)]
enum WriteError {
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serialize: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no stored session {0}")]
    UnknownSession(Ulid),
}

/// The handle side of the store: clones a sender into the single writer
/// thread plus the shared path for blocking reads. Cheap to clone.
#[derive(Clone)]
pub struct HistoryStore {
    tx: mpsc::Sender<Msg>,
    path: Arc<PathBuf>,
}

impl std::fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

const WRITER_GONE: &str = "history writer stopped";

impl HistoryStore {
    /// Opens (creating parents) and migrates the database, opens the writer's
    /// connection, then spawns the single writer thread. Fails when the
    /// database cannot be opened at all.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        migrate(&path)?;
        let conn = Connection::open(&path).map_err(|e| format!("writer open: {e}"))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        // Several server processes share the file: wait for the lock
        // instead of failing the write.
        conn.busy_timeout(WRITER_BUSY_TIMEOUT)
            .map_err(|e| format!("writer busy timeout: {e}"))?;
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .map_err(|e| format!("history writer thread: {e}"))?;
        Ok(Self {
            tx,
            path: Arc::new(path),
        })
    }

    /// Enqueues a fire-and-forget write. Never waits on SQLite.
    fn enqueue(&self, write: Write) {
        if self.tx.send(Msg::Write(Box::new(write), None)).is_err() {
            tracing::error!("history write dropped: {WRITER_GONE}");
        }
    }

    /// Enqueues a write and waits for its outcome.
    async fn acked(&self, write: Write) -> Result<(), String> {
        let (ack, outcome) = oneshot::channel();
        self.tx
            .send(Msg::Write(Box::new(write), Some(ack)))
            .map_err(|_| WRITER_GONE.to_string())?;
        outcome.await.map_err(|_| WRITER_GONE.to_string())?
    }

    /// Barrier: resolves once every write enqueued before this call has been
    /// applied, with the FIRST error any fire-and-forget write has hit since
    /// the store was opened. The failure is sticky: every later flush keeps
    /// returning it until the store is reopened. `Ok` means no write was
    /// ever lost.
    pub async fn flush(&self) -> Result<(), String> {
        let (reply, outcome) = oneshot::channel();
        self.tx
            .send(Msg::Flush(reply))
            .map_err(|_| WRITER_GONE.to_string())?;
        outcome.await.map_err(|_| WRITER_GONE.to_string())?
    }

    /// Registers a session row (INSERT OR IGNORE: a resumed id already has
    /// its row) and waits for the commit: a session whose row cannot be
    /// written would later lose every event and snapshot.
    pub async fn record_session(
        &self,
        id: Ulid,
        workspace: &str,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), String> {
        self.acked(Write::RecordSession {
            id,
            workspace: workspace.to_string(),
            model: model.to_string(),
            profile: profile.map(str::to_string),
        })
        .await
    }

    /// Appends one durable envelope (the caller filters `llm.delta`) and
    /// bumps `last_seq` in the same transaction. Fire-and-forget: a failure
    /// surfaces at the next [`HistoryStore::flush`].
    pub fn append_event(&self, id: Ulid, envelope: EventEnvelope) {
        self.enqueue(Write::AppendEvent { id, envelope });
    }

    /// Snapshots the actor transcript after a turn end (also bumps
    /// `updated_at`; `last_seq` is owned by `append_event`). Fire-and-forget:
    /// a failure surfaces at the next [`HistoryStore::flush`].
    pub fn save_transcript(&self, id: Ulid, transcript: Vec<ChatMessage>) {
        self.enqueue(Write::SaveTranscript { id, transcript });
    }

    /// Sets the session title (first driver message; only when unset).
    /// Fire-and-forget, like [`HistoryStore::append_event`].
    pub fn set_title(&self, id: Ulid, title: String) {
        self.enqueue(Write::SetTitle { id, title });
    }

    /// Removes a session row and its events in one transaction, and waits
    /// for the commit.
    pub async fn delete(&self, id: Ulid) -> Result<(), String> {
        self.acked(Write::Delete { id }).await
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

/// How long the writer waits on a lock held by another server process.
const WRITER_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The single writer thread: one connection, one message at a time, FIFO.
/// A failed write is reported to its ack, or else remembered (first one
/// wins) and reported by every later flush, since a lost write leaves a
/// hole that a clean flush would hide; it is always logged.
fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Msg>) {
    let mut first_error: Option<String> = None;
    for msg in rx {
        match msg {
            Msg::Write(write, ack) => {
                let outcome = apply(&mut conn, *write).map_err(|e| e.to_string());
                if let Err(err) = &outcome {
                    tracing::error!("history write failed: {err}");
                }
                match ack {
                    Some(ack) => {
                        let _ = ack.send(outcome);
                    }
                    None => {
                        if let Err(err) = outcome {
                            first_error.get_or_insert(err);
                        }
                    }
                }
            }
            Msg::Flush(reply) => {
                let _ = reply.send(first_error.clone().map_or(Ok(()), Err));
            }
        }
    }
}

/// Applies one write as ONE transaction: it commits whole or leaves the
/// database untouched.
fn apply(conn: &mut Connection, write: Write) -> Result<(), WriteError> {
    let tx = conn.transaction()?;
    match write {
        Write::RecordSession {
            id,
            workspace,
            model,
            profile,
        } => {
            let now = now_rfc3339();
            tx.execute(
                "INSERT OR IGNORE INTO sessions
                 (id, workspace, model, profile, title, created_at, updated_at, last_seq, transcript)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, -1, NULL)",
                params![id.to_string(), workspace, model, profile, now],
            )?;
        }
        Write::AppendEvent { id, envelope } => {
            let payload = serde_json::to_string(&envelope)?;
            tx.execute(
                "INSERT OR IGNORE INTO events (session_id, seq, kind, envelope)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id.to_string(), envelope.seq as i64, envelope.kind, payload],
            )?;
            let touched = tx.execute(
                "UPDATE sessions SET updated_at = ?2, last_seq = MAX(last_seq, ?3)
                 WHERE id = ?1",
                params![id.to_string(), now_rfc3339(), envelope.seq as i64],
            )?;
            if touched == 0 {
                // Dropping the transaction rolls the orphan event back.
                return Err(WriteError::UnknownSession(id));
            }
        }
        Write::SaveTranscript { id, transcript } => {
            let payload = serde_json::to_string(&transcript)?;
            let touched = tx.execute(
                "UPDATE sessions SET transcript = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), payload, now_rfc3339()],
            )?;
            if touched == 0 {
                return Err(WriteError::UnknownSession(id));
            }
        }
        Write::SetTitle { id, title } => {
            tx.execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1 AND title IS NULL",
                params![id.to_string(), title],
            )?;
        }
        Write::Delete { id } => {
            tx.execute(
                "DELETE FROM events WHERE session_id = ?1",
                params![id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![id.to_string()],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
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

/// Wraps a session's [`SessionBus`] as the sequencer's emitter and forwards
/// every DURABLE envelope — everything except `llm.delta` (live-only, never
/// stored), `session.end` included — to the writer thread, in seq order, synchronously inside the single sequencer task. The
/// hand-off is a channel send: the sequencer never waits on SQLite.
pub struct PersistingEmitter {
    bus: crate::bus::SessionBus,
    session_id: Ulid,
    store: HistoryStore,
}

impl PersistingEmitter {
    pub fn new(session_id: Ulid, bus: crate::bus::SessionBus, store: HistoryStore) -> Self {
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
        if kind != "llm.delta" {
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
    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
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

    /// Polls until the session row carries a title (the writer drained the
    /// set_title command).
    async fn poll_title(store: &HistoryStore, id: Ulid) -> LoadedSession {
        for _ in 0..200 {
            if let Some(loaded) = store.load(id).await.expect("load") {
                if loaded.meta.title.is_some() {
                    return loaded;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("history writer never set the title");
    }

    /// Polls the (fire-and-forget) writer until `list` returns `min` rows:
    /// the channel offers no flush barrier, so tests observe drain by read.
    async fn poll_list(store: &HistoryStore, workspace: &str, min: usize) -> Vec<ListedSession> {
        for _ in 0..200 {
            let listed = store.list(workspace).await.expect("list");
            if listed.len() >= min {
                return listed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("history writer never drained");
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
        store
            .record_session(id, "/w", "cognition", Some("navigator"))
            .await
            .expect("record");
        store
            .record_session(id2, "/w", "default", None)
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.append_event(id, envelope(1, EventKind::MessageReceived));
        store.save_transcript(
            id,
            vec![ChatMessage::User {
                content: "hi".into(),
            }],
        );
        // id2 updated later: it must list first even though id was created
        // first. When both stamps share a millisecond the order between them
        // is undefined, so the assertion runs only on a real timestamp gap.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.append_event(id2, envelope(0, EventKind::SessionStart));
        store.flush().await.expect("flush");

        let listed = poll_list(&store, "/w", 2).await;
        assert_eq!(listed.len(), 2);
        if listed[0].meta.updated_at != listed[1].meta.updated_at {
            assert_eq!(listed[0].meta.id, id2, "newest updated_at first");
        } else {
            // Same-millisecond stamps (a starved writer processed both
            // bursts together): order between them is undefined — both must
            // still be present.
            assert!(
                listed.iter().any(|row| row.meta.id == id)
                    && listed.iter().any(|row| row.meta.id == id2),
                "both sessions listed"
            );
        }

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
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        store.set_title(id, "first".into());
        store.set_title(id, "second".into());
        store.flush().await.expect("flush");
        let loaded = poll_title(&store, id).await;
        assert_eq!(loaded.meta.title.as_deref(), Some("first"));
    }

    // Given a store with a session
    // When delete runs
    // Then list and load no longer see it
    #[tokio::test]
    async fn delete_removes_row_and_events() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        // Wait for the row to EXIST first (the channel is fire-and-forget,
        // so observing the insert proves the earlier commands drained).
        for _ in 0..200 {
            if store.load(id).await.expect("load").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        store.delete(id).await.expect("delete");
        // Then wait for it to be GONE.
        for _ in 0..200 {
            if store.load(id).await.expect("load").is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(store.load(id).await.expect("load").is_none());
        assert!(store.list("/w").await.expect("list").is_empty());
    }

    // Given a store
    // When list runs for another workspace
    // Then it is empty (per-workspace scoping)
    #[tokio::test]
    async fn list_is_per_workspace() {
        let (_dir, store) = temp_store();
        store
            .record_session(Ulid::new(), "/a", "m", None)
            .await
            .expect("record");
        let listed = poll_list(&store, "/a", 1).await;
        assert_eq!(listed.len(), 1);
        assert!(store.list("/b").await.expect("list").is_empty());
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
        b.record_session(Ulid::new(), "/w", "m", None)
            .await
            .expect("record");
        let listed = poll_list(&b, "/w", 1).await;
        assert_eq!(listed.len(), 1);
    }

    /// Opens a second connection on the store's file, the way another
    /// server process (or a corrupting accident) would.
    fn side_connection(dir: &tempfile::TempDir) -> Connection {
        Connection::open(dir.path().join("sessions.db")).expect("side connection")
    }

    // Given a store whose events table has been destroyed underneath it
    // When an event is appended and the store is flushed
    // Then the flush reports the failure (it is not only a log line)
    #[tokio::test]
    async fn a_failed_event_insert_surfaces_at_flush() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        side_connection(&dir)
            .execute_batch("DROP TABLE events")
            .expect("drop events");

        store.append_event(id, envelope(0, EventKind::SessionStart));
        let err = store.flush().await.expect_err("the insert must fail");
        assert!(err.contains("events"), "names the cause: {err}");
    }

    // Given a store whose events table has been destroyed underneath it
    // When an event is appended and the store is flushed several times
    // Then every flush keeps reporting the failure: the history has a hole
    //      that no later flush may hide
    #[tokio::test]
    async fn failed_append_poison_survives_multiple_flushes() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        side_connection(&dir)
            .execute_batch("DROP TABLE events")
            .expect("drop events");

        store.append_event(id, envelope(0, EventKind::SessionStart));
        let first = store.flush().await.expect_err("the insert must fail");
        let second = store.flush().await.expect_err("still poisoned");
        let third = store.flush().await.expect_err("and still");
        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    // Given a store whose sessions table has been destroyed underneath it
    // When a session is recorded
    // Then the acknowledgement carries the error to the caller
    #[tokio::test]
    async fn record_session_reports_a_failed_insert() {
        let (dir, store) = temp_store();
        side_connection(&dir)
            .execute_batch("DROP TABLE sessions")
            .expect("drop sessions");

        let result = store.record_session(Ulid::new(), "/w", "m", None).await;
        assert!(result.is_err(), "the ack must carry the failure");
    }

    // Given a store with a session
    // When two snapshots race a failing write
    // Then flush reports only the FIRST error
    #[tokio::test]
    async fn flush_reports_the_first_error_only() {
        let (_dir, store) = temp_store();
        let known = Ulid::new();
        let unknown_a = Ulid::new();
        let unknown_b = Ulid::new();
        store
            .record_session(known, "/w", "m", None)
            .await
            .expect("record");
        store.save_transcript(unknown_a, vec![]);
        store.save_transcript(known, vec![]);
        store.save_transcript(unknown_b, vec![]);

        let err = store.flush().await.expect_err("first failure surfaces");
        assert!(err.contains(&unknown_a.to_string()), "first error: {err}");
    }

    // Given an event for a session that has no row
    // When it is appended
    // Then the write fails AND leaves no orphan event behind: the event
    //      insert and the last_seq update are one transaction
    #[tokio::test]
    async fn event_and_last_seq_commit_together_or_not_at_all() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.flush().await.expect_err("no session row");

        let orphans: i64 = side_connection(&dir)
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count");
        assert_eq!(orphans, 0, "the failed write was rolled back");
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
        assert_eq!(
            title_from_message(mid),
            "see <editor-context>x</editor-context> then report"
        );
        // Unclosed block: kept verbatim.
        let unclosed = "hello <editor-context> junk";
        assert_eq!(title_from_message(unclosed), "hello <editor-context> junk");
    }
}
