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
//!   title TEXT, created_at TEXT, updated_at TEXT, last_seq INTEGER, transcript TEXT,
//!   model_resolution TEXT, variant TEXT, variant_override TEXT, primary_agent TEXT, reasoning_floor INTEGER)`
//! - `events(session_id TEXT, seq INTEGER, kind TEXT, envelope TEXT,
//!   PRIMARY KEY(session_id, seq))`
//!
//! Stored events: every durable envelope in order, `session.end` included.
//! Recording it keeps `last_seq` at the highest seq any client has seen, so a
//! resumed session continues after it and never reuses a seq; the resume path
//! (not the store) keeps it from replaying to subscribers, so the resumed
//! stream does not look closed. `llm.delta` and `context.updated` are live-only and never stored.
//! `transcript` is the JSON of the actor's `Vec<ChatMessage>`, snapshotted
//! after every turn end.
//!
//! `model_resolution` is NULL or the versioned JSON of a
//! [`ModelResolution`](crate::provider::catalog::ModelResolution): the model
//! identity and bounds the conversation runs on. It is written once (at
//! creation, or at the first activation of a row that has none) and never
//! overwritten: a catalog refresh does not move a conversation.
//!
//! `variant` is NULL or the name of the model variant the conversation
//! runs with: NULL means none (a row from before the column has none). It
//! changes only through [`HistoryStore::set_variant`]. `variant_override` is
//! NULL or the JSON object the variant sent when it was chosen, written
//! together with `variant` (NULL for a row from before the column), so a
//! resume can send the same keys when the offer cannot be read.
//!
//! `checkpoint` is NULL (no compaction) or the versioned JSON of a
//! [`Compaction`]: the summary that stands for the start of the conversation
//! and the tool outputs the prompt shows as markers. `transcript` stays the
//! full history; both are written together by
//! [`HistoryStore::save_checkpoint`]. A resume validates the indices against
//! the transcript, and a non-null value that does not validate fails it.
//!
//! `primary_agent` is NULL (the `build` agent, also for every row written
//! before the column existed) or the id of the primary agent the session runs
//! as. It changes with `/agent`, and the change is committed before it is
//! reported.
//!
//! `reasoning_floor` is NULL (zero) or the transcript index below which the
//! request projection leaves reasoning out: the messages written before a
//! `/model` switch to another model identity. It is written with the model,
//! resolution and variant of the switch, in the same transaction.

mod read;

pub use read::Resumed;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::oneshot;
use ulid::Ulid;

use crate::agent::compaction::Compaction;
use crate::bus::event::{EventEnvelope, EventKind};
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
    /// The raw `model_resolution` column: `None` for a row that has none
    /// (created before the column existed). Parsed by the session service,
    /// so an invalid value fails the resume instead of being dropped here.
    pub model_resolution: Option<String>,
    /// The saved model variant name; `None` means no variant.
    pub variant: Option<String>,
    /// The raw JSON of the saved variant's override object; `None` for a row
    /// saved before it was kept, or with no variant.
    pub variant_override: Option<String>,
    /// The raw `primary_agent` column: `None` is the `build` agent.
    pub primary_agent: Option<String>,
    /// Transcript messages before this index come from a model the
    /// conversation no longer talks to: the request projection leaves their
    /// reasoning out. `None` (a row never switched) is zero.
    pub reasoning_floor: Option<u64>,
}

/// A full session load: the row's metadata, its stored envelopes (in seq
/// order, any `session.end` included), and the last transcript snapshot.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub meta: StoredSession,
    pub envelopes: Vec<EventEnvelope>,
    pub transcript: Vec<ChatMessage>,
    /// The installed compaction, validated against `transcript`.
    pub compaction: Option<Compaction>,
}

/// Durable ownership and completion metadata for one background child.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildLink {
    pub parent_id: Ulid,
    pub child_id: Ulid,
    pub agent: String,
    pub goal: String,
    pub policy_snapshot: String,
    pub status: String,
    pub created_at: String,
    pub terminal_at: Option<String>,
    pub terminal_reason: Option<String>,
    pub result_text: Option<String>,
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
        resolution: Option<String>,
        variant: Option<String>,
        variant_override: Option<String>,
        primary_agent: Option<String>,
    },
    SetVariant {
        id: Ulid,
        variant: Option<String>,
        variant_override: Option<String>,
    },
    SetResolution {
        id: Ulid,
        resolution: String,
    },
    SwitchModel {
        id: Ulid,
        model: String,
        resolution: String,
        variant: Option<String>,
        variant_override: Option<String>,
        reasoning_floor: i64,
        /// The sequenced `model.changed` stored with the new selection.
        envelope: EventEnvelope,
    },
    FillEndpoint {
        id: Ulid,
        expected: String,
        resolution: String,
    },
    SetPrimaryAgent {
        id: Ulid,
        agent: Option<String>,
    },
    AppendEvent {
        id: Ulid,
        envelope: EventEnvelope,
    },
    SaveTranscript {
        id: Ulid,
        transcript: Vec<ChatMessage>,
    },
    SaveCheckpoint {
        id: Ulid,
        transcript: Vec<ChatMessage>,
        checkpoint: String,
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
    #[error("the saved model resolution of session {0} changed while it was being completed")]
    ResolutionChanged(Ulid),
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

    /// Commits the parent/child relationship after both sessions are stored.
    pub async fn create_child_link(
        &self,
        child_id: Ulid,
        agent: String,
        goal: String,
        policy_snapshot: String,
        capacity: usize,
        envelope: EventEnvelope,
    ) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let parent_id = envelope.session_id;
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            let inserted = tx.execute(
                "INSERT INTO child_links (parent_id, child_id, agent, goal, policy_snapshot, status, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, 'running', ?6
                 WHERE (SELECT COUNT(*) FROM child_links WHERE parent_id=?1 AND status IN ('starting','running')) < ?7
                   AND EXISTS(SELECT 1 FROM sessions WHERE id=?1)
                   AND EXISTS(SELECT 1 FROM sessions WHERE id=?2)",
                params![parent_id.to_string(), child_id.to_string(), agent, goal,
                    policy_snapshot, now_rfc3339(), capacity as i64],
            ).map_err(|e| e.to_string())?;
            if inserted == 0 { return Err("subagent capacity reached or parent/child session is missing".into()); }
            insert_committed_event(&tx, &envelope)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(())
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn active_child_count(&self, parent_id: Ulid) -> Result<usize, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM child_links WHERE parent_id=?1 AND status IN ('starting','running')", [parent_id.to_string()], |row| row.get(0)).map_err(|e| e.to_string())?;
            usize::try_from(count).map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
    }

    pub fn active_child_counts(&self) -> Result<std::collections::HashMap<Ulid, usize>, String> {
        let conn = Connection::open(&*self.path).map_err(|e| e.to_string())?;
        let mut statement = conn.prepare("SELECT parent_id, COUNT(*) FROM child_links WHERE status IN ('starting','running') GROUP BY parent_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let (id, count) = row.map_err(|e| e.to_string())?;
            Ok((
                id.parse().map_err(|e| format!("parent id: {e}"))?,
                usize::try_from(count).map_err(|e| e.to_string())?,
            ))
        })
        .collect()
    }

    /// Performs the single terminal compare-and-set for a child. Only the
    /// first completion wins, together with its pending completion notice.
    pub async fn finish_child_link(
        &self,
        child_id: Ulid,
        status: String,
        reason: Option<String>,
        result: String,
    ) -> Result<Option<ChildLink>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            let terminal_at = now_rfc3339();
            let changed = tx.execute(
                "UPDATE child_links SET status=?2, terminal_at=?3, terminal_reason=?4, result_text=?5
                 WHERE child_id=?1 AND status='running'",
                params![child_id.to_string(), status, terminal_at, reason, result],
            ).map_err(|e| e.to_string())?;
            if changed == 0 { tx.rollback().map_err(|e| e.to_string())?; return Ok(None); }
            let row = tx.query_row(
                "SELECT parent_id, child_id, agent, goal, policy_snapshot, status, created_at, terminal_at, terminal_reason, result_text
                 FROM child_links WHERE child_id=?1",
                [child_id.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?, row.get::<_, Option<String>>(9)?)),
            ).map_err(|e| e.to_string())?;
            let outcome = match status.as_str() {
                "done" => crate::bus::event::SubagentOutcome::Done,
                "failed" => crate::bus::event::SubagentOutcome::Failed,
                _ => crate::bus::event::SubagentOutcome::Stopped,
            };
            let payload = serde_json::to_string(&crate::bus::event::SubagentDone {
                parent_session_id: row.0.parse().map_err(|e| format!("parent id: {e}"))?,
                child_session_id: child_id, agent: row.2.clone(), outcome,
                reason: reason.clone(), notification_id: Ulid::new(),
            }).map_err(|e| e.to_string())?;
            tx.execute("INSERT INTO completion_outbox (child_id, parent_id, outcome, result_ref, payload, result_text) VALUES (?1, ?2, ?3, ?1, ?4, ?5)",
                params![child_id.to_string(), row.0, status, payload, result]).map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Some(ChildLink {
                parent_id: row.0.parse().map_err(|e| format!("parent id: {e}"))?,
                child_id: row.1.parse().map_err(|e| format!("child id: {e}"))?,
                agent: row.2, goal: row.3, policy_snapshot: row.4, status: row.5,
                created_at: row.6, terminal_at: row.7, terminal_reason: row.8, result_text: row.9,
            }))
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn child_link(&self, child_id: Ulid) -> Result<Option<ChildLink>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.query_row(
                "SELECT parent_id, child_id, agent, goal, policy_snapshot, status, created_at, terminal_at, terminal_reason, result_text
                 FROM child_links WHERE child_id=?1", [child_id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?, row.get::<_, Option<String>>(7)?, row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?))
                }
            ).optional().map_err(|e| e.to_string())?.map(|row| Ok(ChildLink {
                parent_id: row.0.parse().map_err(|e| format!("parent id: {e}"))?,
                child_id: row.1.parse().map_err(|e| format!("child id: {e}"))?,
                agent: row.2, goal: row.3, policy_snapshot: row.4, status: row.5,
                created_at: row.6, terminal_at: row.7, terminal_reason: row.8, result_text: row.9,
            })).transpose()
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn pending_completions(
        &self,
        parent: Ulid,
    ) -> Result<Vec<(Ulid, serde_json::Value)>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare("SELECT child_id, payload FROM completion_outbox WHERE parent_id=?1 AND delivered=0 ORDER BY rowid").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([parent.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            rows.map(|row| {
                let (child, payload) = row.map_err(|e| e.to_string())?;
                Ok((child.parse().map_err(|e| format!("child id: {e}"))?, serde_json::from_str(&payload).map_err(|e| e.to_string())?))
            }).collect()
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn deliver_completion(
        &self,
        child: Ulid,
        envelope: EventEnvelope,
    ) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            let changed = tx.execute("UPDATE completion_outbox SET delivered=1 WHERE child_id=?1 AND parent_id=?2 AND delivered=0", params![child.to_string(), envelope.session_id.to_string()]).map_err(|e| e.to_string())?;
            if changed == 0 { return Err("completion already delivered".into()); }
            insert_committed_event(&tx, &envelope)?;
            tx.commit().map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn unconsumed_completions(
        &self,
        parent: Ulid,
    ) -> Result<Vec<(Ulid, serde_json::Value, String)>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare("SELECT child_id, payload, result_text FROM completion_outbox WHERE parent_id=?1 AND consumed=0 ORDER BY rowid").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([parent.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(|e| e.to_string())?;
            rows.map(|row| {
                let (child, payload, result) = row.map_err(|e| e.to_string())?;
                Ok((child.parse().map_err(|e| format!("child id: {e}"))?, serde_json::from_str(&payload).map_err(|e| e.to_string())?, result))
            }).collect()
        }).await.map_err(|e| e.to_string())?
    }

    /// Installs the transcript and consumes precisely its appended notices.
    pub async fn consume_completions(
        &self,
        parent: Ulid,
        children: Vec<Ulid>,
        transcript: Vec<ChatMessage>,
    ) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            for child in children {
                let changed = tx.execute("UPDATE completion_outbox SET consumed=1 WHERE child_id=?1 AND parent_id=?2 AND consumed=0", params![child.to_string(), parent.to_string()]).map_err(|e| e.to_string())?;
                if changed != 1 { return Err("completion already consumed or missing".into()); }
            }
            let changed = tx.execute("UPDATE sessions SET transcript=?2, updated_at=?3 WHERE id=?1", params![parent.to_string(), serde_json::to_string(&transcript).map_err(|e| e.to_string())?, now_rfc3339()]).map_err(|e| e.to_string())?;
            if changed != 1 { return Err("completion parent is missing".into()); }
            tx.commit().map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
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
        self.record_session_resolved(id, workspace, model, profile, None, None, None)
            .await
    }

    /// [`HistoryStore::record_session`] with the model resolution saved in
    /// the same insert, and the primary agent when it is not `build`. An
    /// existing row keeps what it has.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_session_resolved(
        &self,
        id: Ulid,
        workspace: &str,
        model: &str,
        profile: Option<&str>,
        resolution: Option<&crate::provider::catalog::ModelResolution>,
        variant: Option<&(String, crate::provider::config::Variant)>,
        primary_agent: Option<&str>,
    ) -> Result<(), String> {
        let (variant, variant_override) = split_variant(variant)?;
        let resolution = resolution
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("serialize model resolution: {e}"))?;
        self.acked(Write::RecordSession {
            id,
            workspace: workspace.to_string(),
            model: model.to_string(),
            profile: profile.map(str::to_string),
            resolution,
            variant,
            variant_override,
            primary_agent: primary_agent.map(str::to_string),
        })
        .await
    }

    /// Saves the model variant of a session with its override object
    /// (`None` clears both) and waits for the commit.
    pub async fn set_variant(
        &self,
        id: Ulid,
        variant: Option<&(String, crate::provider::config::Variant)>,
    ) -> Result<(), String> {
        let (variant, variant_override) = split_variant(variant)?;
        self.acked(Write::SetVariant {
            id,
            variant,
            variant_override,
        })
        .await
    }

    /// Moves a session to another model alias in ONE transaction: the alias,
    /// its resolution, its variant (with the override object), the reasoning
    /// floor and the sequenced `model.changed` event change together or not at
    /// all (an event row already at that seq fails the whole write). Waits for
    /// the commit.
    pub async fn switch_model(
        &self,
        id: Ulid,
        alias: &str,
        resolution: &crate::provider::catalog::ModelResolution,
        variant: Option<&(String, crate::provider::config::Variant)>,
        reasoning_floor: usize,
        envelope: EventEnvelope,
    ) -> Result<(), String> {
        let (variant, variant_override) = split_variant(variant)?;
        let resolution = serde_json::to_string(resolution)
            .map_err(|e| format!("serialize model resolution: {e}"))?;
        self.acked(Write::SwitchModel {
            id,
            model: alias.to_string(),
            resolution,
            variant,
            variant_override,
            reasoning_floor: i64::try_from(reasoning_floor).unwrap_or(i64::MAX),
            envelope,
        })
        .await
    }

    /// Saves the model resolution of a row that has none (a conversation
    /// from before the column). Never overwrites a saved one. Waits for the
    /// commit.
    pub async fn set_model_resolution(
        &self,
        id: Ulid,
        resolution: &crate::provider::catalog::ModelResolution,
    ) -> Result<(), String> {
        let resolution = serde_json::to_string(resolution)
            .map_err(|e| format!("serialize model resolution: {e}"))?;
        self.acked(Write::SetResolution { id, resolution }).await
    }

    /// Replaces a saved resolution that lacks its endpoint with the same one
    /// carrying it. Only when the column still holds exactly `expected`, so a
    /// concurrent change is never overwritten. Waits for the commit.
    pub async fn fill_resolution_endpoint(
        &self,
        id: Ulid,
        expected: &str,
        resolution: &crate::provider::catalog::ModelResolution,
    ) -> Result<(), String> {
        let resolution = serde_json::to_string(resolution)
            .map_err(|e| format!("serialize model resolution: {e}"))?;
        self.acked(Write::FillEndpoint {
            id,
            expected: expected.to_string(),
            resolution,
        })
        .await
    }

    /// Sets the primary agent of a session (`None` is `build`) and waits for
    /// the commit.
    pub async fn set_primary_agent(&self, id: Ulid, agent: Option<&str>) -> Result<(), String> {
        self.acked(Write::SetPrimaryAgent {
            id,
            agent: agent.map(str::to_string),
        })
        .await
    }

    /// Appends one durable envelope (the caller filters the live-only kinds) and
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

    /// Saves the transcript and the compaction that goes with it in ONE
    /// transaction and waits for the commit. The caller installs the
    /// compaction only after this returns `Ok`.
    pub async fn save_checkpoint(
        &self,
        id: Ulid,
        transcript: Vec<ChatMessage>,
        compaction: &Compaction,
    ) -> Result<(), String> {
        self.acked(Write::SaveCheckpoint {
            id,
            transcript,
            checkpoint: compaction.to_json(),
        })
        .await
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
}

/// The two columns a selected variant is saved in: its name and its override
/// object as JSON.
fn split_variant(
    variant: Option<&(String, crate::provider::config::Variant)>,
) -> Result<(Option<String>, Option<String>), String> {
    let Some((name, object)) = variant else {
        return Ok((None, None));
    };
    let raw = serde_json::to_string(object).map_err(|e| format!("serialize variant: {e}"))?;
    Ok((Some(name.clone()), Some(raw)))
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
fn insert_committed_event(
    tx: &rusqlite::Transaction<'_>,
    envelope: &EventEnvelope,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, ?3, ?4)",
        params![
            envelope.session_id.to_string(),
            envelope.seq as i64,
            envelope.kind,
            serde_json::to_string(envelope).map_err(|e| e.to_string())?
        ],
    )
    .map_err(|e| e.to_string())?;
    let touched = tx
        .execute(
            "UPDATE sessions SET updated_at=?2, last_seq=MAX(last_seq, ?3) WHERE id=?1",
            params![
                envelope.session_id.to_string(),
                now_rfc3339(),
                envelope.seq as i64
            ],
        )
        .map_err(|e| e.to_string())?;
    if touched != 1 {
        return Err("event owner session is missing".into());
    }
    Ok(())
}

fn apply(conn: &mut Connection, write: Write) -> Result<(), WriteError> {
    let tx = conn.transaction()?;
    match write {
        Write::RecordSession {
            id,
            workspace,
            model,
            profile,
            resolution,
            variant,
            variant_override,
            primary_agent,
        } => {
            let now = now_rfc3339();
            tx.execute(
                "INSERT OR IGNORE INTO sessions
                 (id, workspace, model, profile, title, created_at, updated_at, last_seq, transcript,
                  model_resolution, variant, variant_override, primary_agent)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, -1, NULL, ?6, ?7, ?8, ?9)",
                params![
                    id.to_string(),
                    workspace,
                    model,
                    profile,
                    now,
                    resolution,
                    variant,
                    variant_override,
                    primary_agent
                ],
            )?;
        }
        Write::SetVariant {
            id,
            variant,
            variant_override,
        } => {
            let touched = tx.execute(
                "UPDATE sessions SET variant = ?2, variant_override = ?3 WHERE id = ?1",
                params![id.to_string(), variant, variant_override],
            )?;
            if touched == 0 {
                return Err(WriteError::UnknownSession(id));
            }
        }
        Write::SetResolution { id, resolution } => {
            let touched = tx.execute(
                "UPDATE sessions SET model_resolution = ?2
                 WHERE id = ?1 AND model_resolution IS NULL",
                params![id.to_string(), resolution],
            )?;
            if touched == 0
                && !tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                        params![id.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|v| v != 0)?
            {
                return Err(WriteError::UnknownSession(id));
            }
        }
        Write::SwitchModel {
            id,
            model,
            resolution,
            variant,
            variant_override,
            reasoning_floor,
            envelope,
        } => {
            let touched = tx.execute(
                "UPDATE sessions
                 SET model = ?2, model_resolution = ?3, variant = ?4, variant_override = ?5,
                     reasoning_floor = ?6
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    model,
                    resolution,
                    variant,
                    variant_override,
                    reasoning_floor
                ],
            )?;
            if touched == 0 {
                return Err(WriteError::UnknownSession(id));
            }
            // A plain insert: a row already at this seq is an error, not a
            // reason to keep the selection without its event.
            tx.execute(
                "INSERT INTO events (session_id, seq, kind, envelope) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.to_string(),
                    envelope.seq as i64,
                    envelope.kind,
                    serde_json::to_string(&envelope)?
                ],
            )?;
            tx.execute(
                "UPDATE sessions SET updated_at = ?2, last_seq = MAX(last_seq, ?3) WHERE id = ?1",
                params![id.to_string(), now_rfc3339(), envelope.seq as i64],
            )?;
        }
        Write::FillEndpoint {
            id,
            expected,
            resolution,
        } => {
            // Exactly one row: another process that completed the same
            // legacy resolution first may have recorded a different endpoint,
            // and this activation must not run against an endpoint that is
            // not the committed one.
            let touched = tx.execute(
                "UPDATE sessions SET model_resolution = ?2
                 WHERE id = ?1 AND model_resolution = ?3",
                params![id.to_string(), resolution, expected],
            )?;
            if touched != 1 {
                return Err(WriteError::ResolutionChanged(id));
            }
        }
        Write::SetPrimaryAgent { id, agent } => {
            let touched = tx.execute(
                "UPDATE sessions SET primary_agent = ?2 WHERE id = ?1",
                params![id.to_string(), agent],
            )?;
            if touched == 0 {
                return Err(WriteError::UnknownSession(id));
            }
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
        Write::SaveCheckpoint {
            id,
            transcript,
            checkpoint,
        } => {
            let payload = serde_json::to_string(&transcript)?;
            let touched = tx.execute(
                "UPDATE sessions SET transcript = ?2, checkpoint = ?3, updated_at = ?4
                 WHERE id = ?1",
                params![id.to_string(), payload, checkpoint, now_rfc3339()],
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
            transcript TEXT,
            model_resolution TEXT,
            variant TEXT,
            variant_override TEXT,
            checkpoint TEXT,
            primary_agent TEXT,
            reasoning_floor INTEGER
        );
        CREATE TABLE IF NOT EXISTS events (
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL,
            envelope TEXT NOT NULL,
            PRIMARY KEY (session_id, seq)
        );
        CREATE TABLE IF NOT EXISTS child_links (
            parent_id TEXT NOT NULL,
            child_id TEXT PRIMARY KEY,
            agent TEXT NOT NULL,
            goal TEXT NOT NULL,
            policy_snapshot TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            terminal_at TEXT,
            terminal_reason TEXT,
            result_text TEXT,
            FOREIGN KEY(parent_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(child_id) REFERENCES sessions(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS child_links_parent ON child_links(parent_id, created_at);
        CREATE TABLE IF NOT EXISTS completion_outbox (
            child_id TEXT PRIMARY KEY REFERENCES child_links(child_id) ON DELETE CASCADE,
            parent_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            outcome TEXT NOT NULL,
            result_ref TEXT NOT NULL,
            payload TEXT NOT NULL,
            delivered INTEGER NOT NULL DEFAULT 0
        );",
    )
    .map_err(|e| format!("migrate: {e}"))?;
    let has_consumption = conn
        .prepare("SELECT 1 FROM pragma_table_info('completion_outbox') WHERE name='consumed'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| e.to_string())?;
    if !has_consumption {
        conn.execute_batch("BEGIN;
            ALTER TABLE completion_outbox RENAME TO completion_outbox_legacy;
            CREATE TABLE completion_outbox (
                child_id TEXT PRIMARY KEY,
                parent_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                outcome TEXT NOT NULL, result_ref TEXT NOT NULL, payload TEXT NOT NULL,
                result_text TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0,
                consumed INTEGER NOT NULL DEFAULT 0);
            INSERT INTO completion_outbox (child_id,parent_id,outcome,result_ref,payload,result_text,delivered)
                SELECT o.child_id,o.parent_id,o.outcome,o.result_ref,o.payload,COALESCE(c.result_text,''),o.delivered
                FROM completion_outbox_legacy o LEFT JOIN child_links c ON c.child_id=o.child_id;
            DROP TABLE completion_outbox_legacy; COMMIT;").map_err(|e| e.to_string())?;
    }
    // A database from before `model_resolution`: add the nullable column.
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'model_resolution'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_column {
        conn.execute("ALTER TABLE sessions ADD COLUMN model_resolution TEXT", [])
            .map_err(|e| format!("migrate: {e}"))?;
    }
    // A database from before `variant`: add the nullable column.
    let has_variant = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'variant'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_variant {
        conn.execute("ALTER TABLE sessions ADD COLUMN variant TEXT", [])
            .map_err(|e| format!("migrate: {e}"))?;
    }
    // A database from before `variant_override`: add the nullable column.
    let has_variant_override = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'variant_override'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_variant_override {
        conn.execute("ALTER TABLE sessions ADD COLUMN variant_override TEXT", [])
            .map_err(|e| format!("migrate: {e}"))?;
    }
    // A database from before `checkpoint`: add the nullable column.
    let has_checkpoint = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'checkpoint'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_checkpoint {
        conn.execute("ALTER TABLE sessions ADD COLUMN checkpoint TEXT", [])
            .map_err(|e| format!("migrate: {e}"))?;
    }
    // A database from before `primary_agent`: add the nullable column.
    let has_primary_agent = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'primary_agent'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_primary_agent {
        conn.execute("ALTER TABLE sessions ADD COLUMN primary_agent TEXT", [])
            .map_err(|e| format!("migrate: {e}"))?;
    }
    // A database from before `reasoning_floor`: add the nullable column.
    let has_floor = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'reasoning_floor'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| format!("migrate: {e}"))?;
    if !has_floor {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN reasoning_floor INTEGER",
            [],
        )
        .map_err(|e| format!("migrate: {e}"))?;
    }
    Ok(())
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
/// every DURABLE envelope — everything except the live-only kinds
/// (`llm.delta`, `context.updated`; never stored), `session.end` included — to the writer thread, in seq order, synchronously inside the single sequencer task. The
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
        // An unknown name is stored: only a catalog kind marked live-only is not.
        if EventKind::from_wire(kind).is_none_or(EventKind::is_durable) {
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

    fn subscribe_live(&mut self) -> tokio::sync::mpsc::Receiver<EventEnvelope> {
        self.bus.subscribe_live()
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

    // Given a session with events 0..=4
    // When replay runs with each bound combination
    // Then it returns exactly after < seq <= through in seq order, and a row
    //      that does not decode is an error naming its seq
    #[tokio::test]
    async fn replay_respects_both_bounds_and_rejects_corruption() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        for seq in 1..=4 {
            store.append_event(id, envelope(seq, EventKind::MessageReceived));
        }
        store.flush().await.expect("flush");
        let seqs = |events: Vec<EventEnvelope>| events.iter().map(|e| e.seq).collect::<Vec<_>>();

        let all = store.replay(id, None, 4).await.expect("replay all");
        assert_eq!(seqs(all), vec![0, 1, 2, 3, 4]);
        let tail = store.replay(id, Some(2), 4).await.expect("replay tail");
        assert_eq!(seqs(tail), vec![3, 4]);
        let head = store.replay(id, None, 2).await.expect("replay head");
        assert_eq!(seqs(head), vec![0, 1, 2]);
        let middle = store.replay(id, Some(1), 3).await.expect("replay middle");
        assert_eq!(seqs(middle), vec![2, 3]);
        let none_after = store.replay(id, Some(4), 4).await.expect("empty range");
        assert!(none_after.is_empty());
        let inverted = store.replay(id, Some(3), 2).await.expect("inverted range");
        assert!(inverted.is_empty());
        let unknown = store.replay(Ulid::new(), None, 4).await.expect("unknown");
        assert!(unknown.is_empty());

        side_connection(&dir)
            .execute(
                "UPDATE events SET envelope = 'not json' WHERE session_id = ?1 AND seq = 3",
                params![id.to_string()],
            )
            .expect("corrupt row");
        let err = store
            .replay(id, None, 4)
            .await
            .expect_err("a corrupt row is an error");
        assert!(err.contains("seq 3"), "names the seq: {err}");
        // A range that avoids the bad row still reads.
        let clean = store.replay(id, None, 2).await.expect("clean range");
        assert_eq!(seqs(clean), vec![0, 1, 2]);
    }

    // Given a recorded session
    // When meta and load_for_resume read it
    // Then they return its row and transcript, and None for an unknown id
    #[tokio::test]
    async fn meta_and_resume_load_omit_the_envelopes() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", Some("p"))
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.save_transcript(
            id,
            vec![ChatMessage::User {
                content: "hi".into(),
            }],
        );
        store.flush().await.expect("flush");

        let meta = store.meta(id).await.expect("meta").expect("present");
        assert_eq!(meta.profile.as_deref(), Some("p"));
        assert_eq!(meta.last_seq, 0);
        let (resumed, transcript, compaction) = store
            .load_for_resume(id)
            .await
            .expect("resume")
            .expect("present");
        assert_eq!(resumed.id, id);
        assert_eq!(transcript.len(), 1);
        assert!(compaction.is_none());
        assert!(store.meta(Ulid::new()).await.expect("meta").is_none());
        assert!(store
            .load_for_resume(Ulid::new())
            .await
            .expect("resume")
            .is_none());
    }

    #[tokio::test]
    async fn child_link_terminal_write_is_a_compare_and_set() {
        let (_dir, store) = temp_store();
        let parent = Ulid::new();
        let child = Ulid::new();
        store.record_session(parent, "/w", "m", None).await.unwrap();
        store.record_session(child, "/w", "m", None).await.unwrap();
        store
            .create_child_link(
                child,
                "explore".into(),
                "goal".into(),
                "{}".into(),
                4,
                EventEnvelope::new(parent, 0, EventKind::SubagentSpawned, serde_json::json!({})),
            )
            .await
            .unwrap();
        let first = store
            .finish_child_link(child, "done".into(), None, "answer".into())
            .await
            .unwrap();
        assert_eq!(
            first.as_ref().map(|link| link.status.as_str()),
            Some("done")
        );
        assert!(store
            .finish_child_link(child, "failed".into(), Some("late".into()), "wrong".into(),)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .child_link(child)
                .await
                .unwrap()
                .unwrap()
                .result_text
                .as_deref(),
            Some("answer")
        );
    }

    #[tokio::test]
    async fn pending_completion_is_delivered_once_after_live_subscriber_disconnects() {
        let (_dir, store) = temp_store();
        let parent = Ulid::new();
        let child = Ulid::new();
        store.record_session(parent, "/w", "m", None).await.unwrap();
        store.record_session(child, "/w", "m", None).await.unwrap();
        store
            .create_child_link(
                child,
                "explore".into(),
                "goal".into(),
                "{}".into(),
                4,
                EventEnvelope::new(parent, 0, EventKind::SubagentSpawned, serde_json::json!({})),
            )
            .await
            .unwrap();
        store
            .finish_child_link(child, "done".into(), None, "answer".into())
            .await
            .unwrap();
        let pending = store.pending_completions(parent).await.unwrap();
        assert_eq!(pending.len(), 1);
        let event = EventEnvelope::new(parent, 1, EventKind::SubagentDone, pending[0].1.clone());
        store
            .deliver_completion(child, event.clone())
            .await
            .unwrap();
        assert!(store.deliver_completion(child, event).await.is_err());
        assert!(store.pending_completions(parent).await.unwrap().is_empty());
        assert_eq!(store.replay(parent, None, 1).await.unwrap().len(), 2);
        store.delete(child).await.unwrap();
        let pending = store.unconsumed_completions(parent).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].2, "answer");
        store
            .consume_completions(parent, vec![child], vec![])
            .await
            .unwrap();
        assert!(store
            .consume_completions(parent, vec![child], vec![])
            .await
            .is_err());
        assert!(store
            .unconsumed_completions(parent)
            .await
            .unwrap()
            .is_empty());
    }

    // Given a store whose events table has been destroyed underneath it
    // When an event is appended and the store is flushed several times
    // Then the first flush names the cause, and every later flush keeps
    //      reporting the same failure: the history has a hole that no later
    //      flush may hide
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
        assert!(first.contains("events"), "names the cause: {first}");
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

    // Given a database written before `model_resolution` existed
    // When the store opens it
    // Then the column is added, old rows read with none, and a second open
    //      is a no-op
    #[tokio::test]
    async fn migration_adds_the_resolution_column_to_an_old_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.db");
        let id = Ulid::new();
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL, model TEXT NOT NULL,
                    profile TEXT, title TEXT, created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL, last_seq INTEGER NOT NULL DEFAULT -1,
                    transcript TEXT
                 );
                 CREATE TABLE events (
                    session_id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
                    envelope TEXT NOT NULL, PRIMARY KEY (session_id, seq)
                 );",
            )
            .expect("old schema");
            conn.execute(
                "INSERT INTO sessions VALUES (?1, '/w', 'm', NULL, NULL, 't', 't', -1, NULL)",
                params![id.to_string()],
            )
            .expect("old row");
        }
        let store = HistoryStore::open(&path).expect("migrates");
        let meta = store.meta(id).await.expect("meta").expect("present");
        assert_eq!(meta.model_resolution, None);
        assert_eq!(meta.variant, None, "an old row has no variant");
        assert_eq!(meta.variant_override, None);
        drop(store);
        HistoryStore::open(&path).expect("idempotent");
    }

    // Given a session saved with a variant
    // When the variant is changed, cleared, and set for an unknown id
    // Then each read shows it; the unknown id is an error
    #[tokio::test]
    async fn the_variant_is_saved_changed_and_cleared() {
        let pick = |name: &str| {
            let mut object = crate::provider::config::Variant::new();
            object.insert("reasoning_effort".into(), name.into());
            (name.to_string(), object)
        };
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session_resolved(id, "/w", "m", None, None, Some(&pick("high")), None)
            .await
            .expect("record");
        let read = |store: &HistoryStore| {
            let store = store.clone();
            async move { store.meta(id).await.expect("meta").expect("row").variant }
        };
        assert_eq!(read(&store).await.as_deref(), Some("high"));
        store
            .set_variant(id, Some(&pick("low")))
            .await
            .expect("set");
        assert_eq!(read(&store).await.as_deref(), Some("low"));
        let raw = store.meta(id).await.unwrap().unwrap().variant_override;
        assert_eq!(raw.as_deref(), Some(r#"{"reasoning_effort":"low"}"#));
        store.set_variant(id, None).await.expect("clear");
        assert_eq!(read(&store).await, None);
        assert_eq!(
            store.meta(id).await.unwrap().unwrap().variant_override,
            None
        );
        assert!(store
            .set_variant(Ulid::new(), Some(&pick("x")))
            .await
            .is_err());
        let listed = store.list("/w").await.expect("list");
        assert_eq!(listed[0].meta.variant, None);
    }

    // Given a saved resolution
    // When another is offered for the same row
    // Then the first stays: a snapshot is written once
    #[tokio::test]
    async fn a_saved_resolution_is_never_overwritten() {
        use crate::provider::catalog::{Bounds, ModelResolution, Provenance, RESOLUTION_VERSION};
        let make = |window| ModelResolution {
            version: RESOLUTION_VERSION,
            provider: "p".into(),
            model: "m".into(),
            api: "chat".into(),
            endpoint: "http://h/v1".into(),
            bounds: Bounds {
                context_window: window,
                max_input_tokens: None,
                reported_output: None,
            },
            history_policy: "preserve".into(),
            provenance: Provenance {
                context_window: "models.json".into(),
                max_input_tokens: "none".into(),
                reported_output: "none".into(),
            },
        };
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", None)
            .await
            .expect("record");
        store
            .set_model_resolution(id, &make(100))
            .await
            .expect("first");
        store
            .set_model_resolution(id, &make(200))
            .await
            .expect("ignored");
        store
            .record_session_resolved(id, "/w", "m", None, Some(&make(300)), None, None)
            .await
            .expect("existing row keeps its own");
        let meta = store.meta(id).await.expect("meta").expect("present");
        let saved = ModelResolution::from_saved(&meta.model_resolution.expect("saved")).unwrap();
        assert_eq!(saved.bounds.context_window, 100);
        assert!(store
            .set_model_resolution(Ulid::new(), &make(1))
            .await
            .is_err());
    }

    fn exchange(n: usize) -> Vec<ChatMessage> {
        use crate::provider::openai::{RequestFunction, RequestToolCall};
        vec![
            ChatMessage::User {
                content: format!("request {n}"),
            },
            ChatMessage::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RequestToolCall {
                    id: format!("c{n}"),
                    kind: "function".into(),
                    function: RequestFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            ChatMessage::Tool {
                tool_call_id: format!("c{n}"),
                content: "output".repeat(50),
            },
            ChatMessage::Assistant {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
            },
        ]
    }

    fn compaction(through: usize, pruned: Vec<usize>) -> Compaction {
        Compaction {
            checkpoint: crate::agent::compaction::Checkpoint {
                through_message: through,
                summary: "the gist".into(),
                generation: 1,
            },
            pruned_message_indices: pruned,
        }
    }

    // Given a session with a transcript and no checkpoint
    // When a checkpoint is saved
    // Then transcript and checkpoint read back together, in the versioned shape
    #[tokio::test]
    async fn a_checkpoint_is_saved_with_the_transcript_and_read_back() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "m", None).await.unwrap();
        let transcript: Vec<ChatMessage> = (0..4).flat_map(exchange).collect();
        let k = compaction(3, vec![6]);
        store
            .save_checkpoint(id, transcript.clone(), &k)
            .await
            .unwrap();
        let (_, back, restored) = store.load_for_resume(id).await.unwrap().unwrap();
        assert_eq!(back.len(), transcript.len());
        assert_eq!(restored, Some(k));
        let raw: String = side_connection(&dir)
            .query_row("SELECT checkpoint FROM sessions", [], |r| r.get(0))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["checkpoint"]["through_message"], 3);
        assert_eq!(v["checkpoint"]["summary"], "the gist");
        assert_eq!(v["checkpoint"]["generation"], 1);
        assert_eq!(v["pruned_message_indices"], serde_json::json!([6]));
    }

    // Given a checkpoint for an unknown session
    // When it is saved
    // Then the write fails and nothing is acknowledged
    #[tokio::test]
    async fn saving_a_checkpoint_of_an_unknown_session_fails() {
        let (_dir, store) = temp_store();
        let err = store
            .save_checkpoint(Ulid::new(), vec![], &compaction(0, vec![]))
            .await
            .unwrap_err();
        assert!(err.contains("no stored session"), "{err}");
    }

    // Given a stored checkpoint that does not fit the transcript, or is not JSON
    // When the session is resumed
    // Then the resume fails clearly; a NULL checkpoint resumes as before
    #[tokio::test]
    async fn an_invalid_checkpoint_fails_the_resume() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "m", None).await.unwrap();
        store.save_transcript(id, exchange(0));
        store.flush().await.unwrap();
        assert!(store
            .load_for_resume(id)
            .await
            .unwrap()
            .unwrap()
            .2
            .is_none());
        let conn = side_connection(&dir);
        for bad in [
            "{not json".to_string(),
            compaction(99, vec![]).to_json(),
            compaction(0, vec![2])
                .to_json()
                .replace("\"version\":1", "\"version\":7"),
        ] {
            conn.execute("UPDATE sessions SET checkpoint = ?1", [&bad])
                .unwrap();
            let err = store.load_for_resume(id).await.unwrap_err();
            assert!(err.contains("saved checkpoint"), "{err}");
            assert!(store.load(id).await.is_err());
        }
    }

    // Given a database from before the checkpoint column
    // When it is opened
    // Then the nullable column is added and old rows resume without one
    #[tokio::test]
    async fn migration_adds_the_checkpoint_column_to_an_old_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL, model TEXT NOT NULL,
                    profile TEXT, title TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    last_seq INTEGER NOT NULL DEFAULT -1, transcript TEXT
                 );
                 CREATE TABLE events (
                    session_id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
                    envelope TEXT NOT NULL, PRIMARY KEY (session_id, seq)
                 );
                 INSERT INTO sessions (id, workspace, model, created_at, updated_at)
                 VALUES ('01J0000000000000000000000A', '/w', 'm', 't', 't');",
            )
            .unwrap();
        }
        let store = HistoryStore::open(&path).unwrap();
        let id = Ulid::from_string("01J0000000000000000000000A").unwrap();
        let (_, _, compaction) = store.load_for_resume(id).await.unwrap().unwrap();
        assert!(compaction.is_none());
    }
}
