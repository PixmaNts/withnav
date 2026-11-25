//! The read side of the history store: every read opens its own connection
//! and runs inside ONE read transaction, so a result never mixes two
//! committed states of the database.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use ulid::Ulid;

use super::{HistoryStore, ListedSession, LoadedSession, StoredSession};
use crate::agent::compaction::Compaction;
use crate::bus::event::EventEnvelope;
use crate::provider::openai::ChatMessage;

impl HistoryStore {
    pub async fn agent_overrides(&self, id: Ulid) -> Result<Option<super::AgentOverrides>, String> {
        Ok(self.meta(id).await?.map(|meta| meta.agent_overrides))
    }
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
    /// Sessions of one workspace with their captain message counts, newest
    /// `updated_at` first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<ListedSession>, String> {
        let path = Arc::clone(&self.path);
        let workspace = workspace.to_string();
        blocking(move || list_sync(&path, &workspace)).await
    }

    /// Every workspace with at least one top-level conversation, with its
    /// conversation count and latest `updated_at`; no particular order.
    pub async fn workspaces(&self) -> Result<Vec<super::WorkspaceSummary>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let mut stmt = tx
                    .prepare(
                        "SELECT s.workspace, COUNT(*), MAX(s.updated_at) FROM sessions s
                         WHERE NOT EXISTS (SELECT 1 FROM child_links c WHERE c.child_id = s.id)
                         GROUP BY s.workspace",
                    )
                    .map_err(|e| format!("prepare: {e}"))?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(super::WorkspaceSummary {
                            workspace: row.get(0)?,
                            session_count: row.get::<_, i64>(1)?.max(0) as u64,
                            updated_at: row.get(2)?,
                        })
                    })
                    .map_err(|e| format!("query: {e}"))?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("row: {e}"))
            })
        })
        .await
    }

    /// Metadata of one session, without events or transcript.
    pub async fn meta(&self, id: Ulid) -> Result<Option<StoredSession>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                tx.query_row(
                    "SELECT id, workspace, model, editor, title, created_at, updated_at, last_seq,
                            model_resolution, variant, variant_override, primary_agent,
                            reasoning_floor, agent_overrides, selection_provenance, resume_disabled_reason
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

    /// Metadata and an optional child link from the same read snapshot. A root
    /// has metadata with no link; an unknown session has neither.
    pub async fn meta_with_child_link(
        &self,
        id: Ulid,
    ) -> Result<Option<(StoredSession, Option<super::ChildLink>)>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let row = tx
                    .query_row(
                        "SELECT s.id, s.workspace, s.model, s.editor, s.title, s.created_at,
                            s.updated_at, s.last_seq, s.model_resolution, s.variant,
                            s.variant_override, s.primary_agent, s.reasoning_floor,
                            s.agent_overrides, s.selection_provenance, s.resume_disabled_reason,
                            c.parent_id, c.child_id, c.agent, c.goal, c.policy_snapshot,
                            c.status, c.created_at, c.terminal_at, c.terminal_reason, c.result_text
                     FROM sessions s LEFT JOIN child_links c ON c.child_id = s.id
                     WHERE s.id = ?1",
                        params![id.to_string()],
                        |row| {
                            let meta = row_to_session(row)?;
                            let link = if row.get::<_, Option<String>>(17)?.is_some() {
                                Some(super::child_link_from_row_at(row, 16)?)
                            } else {
                                None
                            };
                            Ok((meta, link))
                        },
                    )
                    .optional()
                    .map_err(|e| format!("meta row: {e}"))?;
                row.map(|(meta, link)| {
                    Ok((meta, link.map(super::ChildLinkRow::decode).transpose()?))
                })
                .transpose()
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

    /// The text of the Captain's latest message in one session: the newest
    /// stored `message.received` whose source is a human ingress identity
    /// (`user:http`, `user:cli`), which also covers an answer to an `ask`. A
    /// navigator wake, delegated text, subagent result or command with a correlated
    /// whole-message skill refusal is not it. `None`
    /// when the Captain never spoke.
    pub async fn latest_captain_message(&self, id: Ulid) -> Result<Option<String>, String> {
        let path = Arc::clone(&self.path);
        blocking(move || {
            with_read_tx(&path, |tx| {
                let mut refusals = tx.prepare("SELECT envelope FROM events WHERE session_id = ?1 AND kind = 'command.result'")
                    .map_err(|e| format!("prepare refusals: {e}"))?;
                let rejected = refusals.query_map(params![id.to_string()], |row| row.get::<_, String>(0))
                    .map_err(|e| format!("query refusals: {e}"))?
                    .map(|row| {
                        let raw = row.map_err(|e| format!("refusal row: {e}"))?;
                        serde_json::from_str::<EventEnvelope>(&raw).map_err(|e| format!("refusal does not decode: {e}"))
                    }).collect::<Result<Vec<_>, String>>()?
                    .into_iter().filter(|event| event.payload["name"] == "skill" && event.payload["ok"] == false)
                    .filter_map(|event| event.correlation_id.filter(|id| event.payload["operation_id"].as_str() == Some(id.to_string().as_str())))
                    .collect::<std::collections::HashSet<_>>();
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
                    if envelope.payload["command_id"].as_str().and_then(|id| id.parse::<Ulid>().ok()).is_some_and(|id| rejected.contains(&id)) {
                        continue;
                    }
                    if source.is_some_and(crate::bus::command::is_captain_source) {
                        if let Some(answer) = envelope.payload.get("answer").filter(|a| !a.is_null()) {
                            let answer = serde_json::from_value::<crate::tools::control::AskAnswer>(answer.clone())
                                .map_err(|e| format!("event seq {seq} answer does not decode: {e}"))?;
                            return Ok(envelope.payload["content"].as_str().map(|text| answer.historical_text(text)));
                        }
                        return Ok(envelope
                            .payload
                            .get("content")
                            .and_then(|v| v.as_str())
                            .map(|text| crate::mcp::captain_prose(text).to_string()));
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
    #[cfg(test)]
    read_count::note(path);
    let mut conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|e| format!("begin: {e}"))?;
    work(&tx)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let id: String = row.get(0)?;
    let editor: bool = row.get(3)?;
    Ok(StoredSession {
        resume_disabled_reason: row.get(15)?,
        provenance: row
            .get::<_, Option<String>>(14)?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
        agent_overrides: super::decode_agent_overrides(
            row.get::<_, Option<String>>(13)?.as_deref(),
        )
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(e)),
            )
        })?,
        id: Ulid::from_string(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        workspace: row.get(1)?,
        model: row.get(2)?,
        editor,
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

fn matching_workspaces(tx: &Transaction<'_>, workspace: &str) -> Result<Vec<String>, String> {
    let mut stmt = tx
        .prepare("SELECT DISTINCT workspace FROM sessions")
        .map_err(|e| format!("prepare distinct workspaces: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query distinct workspaces: {e}"))?;

    let target_path = Path::new(workspace);
    let target_canonical = crate::tools::canonical_path(target_path).ok();

    let mut matches = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for row in rows {
        let stored = row.map_err(|e| e.to_string())?;
        let is_exact = stored == workspace;
        let is_canonical_match = target_canonical.as_ref().is_some_and(|target| {
            crate::tools::canonical_path(Path::new(&stored))
                .ok()
                .as_ref()
                == Some(target)
        });

        if (is_exact || is_canonical_match) && seen.insert(stored.clone()) {
            matches.push(stored);
        }
    }

    if seen.insert(workspace.to_string()) {
        matches.push(workspace.to_string());
    }

    Ok(matches)
}

fn list_sync(path: &Path, workspace: &str) -> Result<Vec<ListedSession>, String> {
    with_read_tx(path, |tx| {
        let matches = matching_workspaces(tx, workspace)?;
        if matches.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (1..=matches.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT s.id, s.workspace, s.model, s.editor, s.title,
                    s.created_at, s.updated_at, s.last_seq,
                    s.model_resolution, s.variant, s.variant_override, s.primary_agent,
                    s.reasoning_floor, s.agent_overrides, s.selection_provenance, s.resume_disabled_reason,
                    (SELECT COUNT(*) FROM events e
                     WHERE e.session_id = s.id AND e.kind = 'message.received'),
                    c.parent_id, c.child_id, c.agent, c.goal, c.policy_snapshot,
                    c.status, c.created_at, c.terminal_at, c.terminal_reason, c.result_text
             FROM sessions s LEFT JOIN child_links c ON c.child_id = s.id
             WHERE s.workspace IN ({placeholders})
             ORDER BY s.updated_at DESC, s.id DESC"
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(&matches), |row| {
                let link = if row.get::<_, Option<String>>(18)?.is_some() {
                    Some(super::child_link_from_row_at(row, 17)?)
                } else {
                    None
                };
                Ok((
                    row_to_session(row)?,
                    row.get::<_, i64>(16)?.max(0) as u64,
                    link,
                ))
            })
            .map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for row in rows {
            let decoded = row
                .map_err(|e| e.to_string())
                .and_then(|(meta, count, link)| {
                    Ok(ListedSession {
                        meta,
                        message_count: count,
                        child_link: link.map(super::ChildLinkRow::decode).transpose()?,
                    })
                });
            match decoded {
                Ok(session) => {
                    if seen_ids.insert(session.meta.id) {
                        out.push(session);
                    }
                }
                // One unreadable row must not hide the healthy conversations of the
                // workspace; opening that row (`meta`, resume) still fails explicitly.
                Err(err) => tracing::error!("history list: unreadable row skipped: {err}"),
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
            "SELECT id, workspace, model, editor, title, created_at, updated_at, last_seq,
                    model_resolution, variant, variant_override, primary_agent, reasoning_floor,
                     agent_overrides, selection_provenance, resume_disabled_reason, transcript, checkpoint
             FROM sessions WHERE id = ?1",
            params![id.to_string()],
            |row| {
                let meta = row_to_session(row)?;
                let transcript: Option<String> = row.get(16)?;
                let checkpoint: Option<String> = row.get(17)?;
                Ok((meta, transcript, checkpoint))
            },
        )
        .optional()
        .map_err(|e| format!("load row: {e}"))?
    else {
        return Ok(None);
    };
    let transcript = match transcript.as_deref() {
        None => Vec::new(),
        Some(raw) => serde_json::from_str::<Vec<ChatMessage>>(raw)
            .map_err(|e| format!("the saved transcript of session {id} cannot be read: {e}"))?,
    };
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

/// Test-only count of read connections opened per database file, so a test can
/// assert how many store reads one operation costs. Keyed by path: parallel tests
/// each use their own temp database.
#[cfg(test)]
pub(crate) mod read_count {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static COUNTS: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

    pub(crate) fn note(path: &Path) {
        let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
        *counts
            .get_or_insert_with(HashMap::new)
            .entry(path.to_path_buf())
            .or_default() += 1;
    }

    pub(crate) fn get(path: &Path) -> usize {
        COUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|counts| counts.get(path).copied())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_listing_matches_canonical_aliases_and_raw_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("history.db");
        let store = HistoryStore::open(&db_path).expect("open history store");

        let real_ws = temp.path().join("real_workspace");
        std::fs::create_dir_all(&real_ws).unwrap();
        let real_canonical = crate::tools::canonical_path(&real_ws).unwrap();

        let symlink_ws = temp.path().join("symlink_alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_ws, &symlink_ws).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real_ws, &symlink_ws).unwrap();

        let missing_ws = temp.path().join("historical_missing_workspace");

        let id_symlink = Ulid::generate();
        let id_real = Ulid::generate();
        let id_missing = Ulid::generate();

        let symlink_str = symlink_ws.to_str().unwrap();
        let real_str = real_canonical.to_str().unwrap();
        let missing_str = missing_ws.to_str().unwrap();

        store
            .record_session(id_symlink, symlink_str, "default", false)
            .await
            .expect("record symlink session");
        store
            .record_session(id_real, real_str, "default", false)
            .await
            .expect("record real session");
        store
            .record_session(id_missing, missing_str, "default", false)
            .await
            .expect("record missing session");

        // Listing by canonical path returns both the real path session and symlink alias session
        let list_real = store.list(real_str).await.expect("list by real");
        let listed_ids: Vec<_> = list_real.iter().map(|s| s.meta.id).collect();
        assert_eq!(listed_ids.len(), 2);
        assert!(listed_ids.contains(&id_symlink));
        assert!(listed_ids.contains(&id_real));
        assert!(!listed_ids.contains(&id_missing));

        // Listing by symlink alias also returns both
        let list_symlink = store.list(symlink_str).await.expect("list by symlink");
        let symlink_listed_ids: Vec<_> = list_symlink.iter().map(|s| s.meta.id).collect();
        assert_eq!(symlink_listed_ids.len(), 2);
        assert!(symlink_listed_ids.contains(&id_symlink));
        assert!(symlink_listed_ids.contains(&id_real));

        // Listing by missing historical path returns the exact raw match
        let list_missing = store.list(missing_str).await.expect("list by missing");
        let missing_listed_ids: Vec<_> = list_missing.iter().map(|s| s.meta.id).collect();
        assert_eq!(missing_listed_ids, vec![id_missing]);

        // Stored rows are never rewritten in the database
        let loaded_symlink = store.meta(id_symlink).await.unwrap().unwrap();
        assert_eq!(loaded_symlink.workspace, symlink_str);
        let loaded_missing = store.meta(id_missing).await.unwrap().unwrap();
        assert_eq!(loaded_missing.workspace, missing_str);
    }
}
