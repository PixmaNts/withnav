//! The read side of the history store: every read opens its own connection
//! and runs inside ONE read transaction, so a result never mixes two
//! committed states of the database.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use ulid::Ulid;

use super::{HistoryStore, ListedSession, LoadedSession, StoredSession};
use crate::agent::compaction::Compaction;
use crate::bus::event::EventEnvelope;
use crate::provider::openai::ChatMessage;

impl HistoryStore {
    /// Progress holds use only typed yield and watchdog-resolution rows; unrelated corrupt
    /// command rows must not hide an outstanding hold on recovery.
    pub async fn watchdog_events(&self, id: Ulid) -> Result<Vec<EventEnvelope>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || with_read_tx(&path, |tx| {
            let mut query = tx.prepare("SELECT envelope FROM events WHERE session_id=?1 AND (kind IN ('agent.yield','message.received') OR (kind='command.result' AND json_valid(envelope) AND json_extract(envelope,'$.payload.name')='watchdog')) ORDER BY seq") .map_err(|e| e.to_string())?;
            let rows = query.query_map(params![id.to_string()], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rows.map(|row| serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| e.to_string())).collect()
        })).await
    }
    /// Sessions of one workspace with their driver message counts, newest
    /// `updated_at` first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<ListedSession>, String> {
        let path = Arc::clone(&self.path);
        let workspace = workspace.to_string();
        blocking(move || list_sync(&path, &workspace)).await
    }

    /// Metadata of one session, without events or transcript.
    pub async fn meta(&self, id: Ulid) -> Result<Option<StoredSession>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                tx.query_row(
                    "SELECT id, workspace, model, profile, title, created_at, updated_at, last_seq,
                            model_resolution, variant, variant_override, primary_agent,
                            reasoning_floor
                     FROM sessions WHERE id = ?1",
                    params![id.to_string()],
                    row_to_session,
                )
                .optional()
                .map_err(|e| format!("meta row: {e}"))
            })
        })
        .await
    }

    /// Stored envelopes with `after < seq <= through`, in seq order (`after`
    /// of `None` starts at the first event), read in one transaction. A row
    /// that does not decode is an error naming its seq, never a gap.
    pub async fn replay(
        &self,
        id: Ulid,
        after: Option<u64>,
        through: u64,
    ) -> Result<Vec<EventEnvelope>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || with_read_tx(&path, |tx| replay_tx(tx, id, after, through))).await
    }

    /// The `permission.requested` and `permission.resolved` envelopes of one
    /// session, in seq order.
    pub async fn permission_events(&self, id: Ulid) -> Result<Vec<EventEnvelope>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let mut stmt = tx
                    .prepare(
                        "SELECT seq, envelope FROM events
                         WHERE session_id = ?1 AND kind IN ('permission.requested', 'permission.resolved')
                         ORDER BY seq ASC",
                    )
                    .map_err(|e| format!("prepare: {e}"))?;
                let rows = stmt
                    .query_map(params![id.to_string()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| format!("query: {e}"))?;
                let mut out = Vec::new();
                for row in rows {
                    let (seq, raw) = row.map_err(|e| format!("row: {e}"))?;
                    out.push(
                        serde_json::from_str::<EventEnvelope>(&raw)
                            .map_err(|e| format!("event seq {seq} does not decode: {e}"))?,
                    );
                }
                Ok(out)
            })
        })
        .await
    }

    /// The `model.changed` envelopes of one session, in seq order.
    pub async fn model_events(&self, id: Ulid) -> Result<Vec<EventEnvelope>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let mut stmt = tx
                    .prepare(
                        "SELECT seq, envelope FROM events
                         WHERE session_id = ?1 AND kind = 'model.changed'
                         ORDER BY seq ASC",
                    )
                    .map_err(|e| format!("prepare: {e}"))?;
                let rows = stmt
                    .query_map(params![id.to_string()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| format!("query: {e}"))?;
                let mut out = Vec::new();
                for row in rows {
                    let (seq, raw) = row.map_err(|e| format!("row: {e}"))?;
                    out.push(
                        serde_json::from_str::<EventEnvelope>(&raw)
                            .map_err(|e| format!("event seq {seq} does not decode: {e}"))?,
                    );
                }
                Ok(out)
            })
        })
        .await
    }

    /// The text of the driver's latest message in one session: the newest
    /// stored `message.received` whose source is a human ingress identity
    /// (`user:http`, `user:cli`), which also covers an answer to an `ask`. A
    /// navigator wake, delegated text or a subagent result is not it. `None`
    /// when the driver never spoke.
    pub async fn latest_driver_message(&self, id: Ulid) -> Result<Option<String>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let mut stmt = tx
                    .prepare(
                        "SELECT seq, envelope FROM events
                         WHERE session_id = ?1 AND kind = 'message.received'
                         ORDER BY seq DESC",
                    )
                    .map_err(|e| format!("prepare: {e}"))?;
                let rows = stmt
                    .query_map(params![id.to_string()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| format!("query: {e}"))?;
                for row in rows {
                    let (seq, raw) = row.map_err(|e| format!("row: {e}"))?;
                    let envelope = serde_json::from_str::<EventEnvelope>(&raw)
                        .map_err(|e| format!("event seq {seq} does not decode: {e}"))?;
                    let source = envelope.payload.get("source").and_then(|v| v.as_str());
                    if source.is_some_and(crate::bus::command::is_driver_source) {
                        return Ok(envelope
                            .payload
                            .get("content")
                            .and_then(|v| v.as_str())
                            .map(str::to_string));
                    }
                }
                Ok(None)
            })
        })
        .await
    }

    /// What a resume needs: the metadata and the last transcript snapshot,
    /// without the envelopes (see [`HistoryStore::replay`]).
    pub async fn load_for_resume(&self, id: Ulid) -> Result<Option<Resumed>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || with_read_tx(&path, |tx| resume_tx(tx, id))).await
    }

    /// A full load: metadata, every stored envelope and the transcript, all
    /// from one read transaction.
    pub async fn load(&self, id: Ulid) -> Result<Option<LoadedSession>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let Some((meta, transcript, compaction)) = resume_tx(tx, id)? else {
                    return Ok(None);
                };
                let envelopes = replay_tx(tx, id, None, meta.last_seq)?;
                Ok(Some(LoadedSession {
                    meta,
                    envelopes,
                    transcript,
                    compaction,
                }))
            })
        })
        .await
    }

    /// Whether the store knows this id (cheap EXISTS probe).
    pub async fn exists(&self, id: Ulid) -> Result<bool, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
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
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("join: {e}"))?
}

/// Opens a fresh connection and runs `work` inside one read transaction.
fn with_read_tx<T>(
    path: &Path,
    work: impl FnOnce(&Transaction<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let mut conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|e| format!("begin: {e}"))?;
    work(&tx)
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
        model_resolution: row.get(8)?,
        variant: row.get(9)?,
        variant_override: row.get(10)?,
        primary_agent: row.get(11)?,
        reasoning_floor: row.get::<_, Option<i64>>(12)?.map(|n| n.max(0) as u64),
    })
}

fn list_sync(path: &Path, workspace: &str) -> Result<Vec<ListedSession>, String> {
    with_read_tx(path, |tx| {
        let mut stmt = tx
            .prepare(
                "SELECT s.id, s.workspace, s.model, s.profile, s.title,
                        s.created_at, s.updated_at, s.last_seq,
                        s.model_resolution, s.variant, s.variant_override, s.primary_agent,
                        s.reasoning_floor,
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
                    message_count: row.get::<_, i64>(13)?.max(0) as u64,
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
    })
}

/// A session row, its transcript and its validated compaction.
pub type Resumed = (StoredSession, Vec<ChatMessage>, Option<Compaction>);

fn resume_tx(tx: &Transaction<'_>, id: Ulid) -> Result<Option<Resumed>, String> {
    let Some((meta, transcript, checkpoint)) = tx
        .query_row(
            "SELECT id, workspace, model, profile, title, created_at, updated_at, last_seq,
                    model_resolution, variant, variant_override, primary_agent, reasoning_floor,
                    transcript, checkpoint
             FROM sessions WHERE id = ?1",
            params![id.to_string()],
            |row| {
                let meta = row_to_session(row)?;
                let transcript: Option<String> = row.get(13)?;
                let checkpoint: Option<String> = row.get(14)?;
                Ok((meta, transcript, checkpoint))
            },
        )
        .optional()
        .map_err(|e| format!("load row: {e}"))?
    else {
        return Ok(None);
    };
    let transcript = transcript
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<ChatMessage>>(raw).ok())
        .unwrap_or_default();
    let compaction = checkpoint
        .as_deref()
        .map(|raw| Compaction::from_json(raw, &transcript))
        .transpose()
        .map_err(|e| format!("the saved checkpoint of session {id} cannot be used: {e}"))?;
    Ok(Some((meta, transcript, compaction)))
}

fn replay_tx(
    tx: &Transaction<'_>,
    id: Ulid,
    after: Option<u64>,
    through: u64,
) -> Result<Vec<EventEnvelope>, String> {
    // Seqs are stored as i64: an `after` of `None` sits below the first seq.
    let after = after.map_or(-1, |seq| i64::try_from(seq).unwrap_or(i64::MAX));
    let through = i64::try_from(through).unwrap_or(i64::MAX);
    let mut stmt = tx
        .prepare(
            "SELECT seq, envelope FROM events
             WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3
             ORDER BY seq ASC",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![id.to_string(), after, through], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut envelopes = Vec::new();
    for row in rows {
        let (seq, raw) = row.map_err(|e| format!("row: {e}"))?;
        let envelope = serde_json::from_str::<EventEnvelope>(&raw)
            .map_err(|e| format!("event seq {seq} does not decode: {e}"))?;
        envelopes.push(envelope);
    }
    Ok(envelopes)
}
