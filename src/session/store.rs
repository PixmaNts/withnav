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
//!   model_resolution TEXT, variant TEXT, variant_override TEXT, primary_agent TEXT, reasoning_floor INTEGER,
//!   resume_disabled_reason TEXT)`
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
//! `resume_disabled_reason` is NULL for ordinary conversations, or safe text
//! explicitly marking a read-only archive. Missing model resolution alone never
//! marks an archive. The marker leaves metadata, transcript and events intact.
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

/// Session-local choice; inheritance and explicitly disabling a variant are distinct.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "name",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentVariantOverride {
    #[default]
    Inherit,
    Off,
    Named(String),
}

impl From<super::VariantChoice> for AgentVariantOverride {
    fn from(choice: super::VariantChoice) -> Self {
        match choice {
            super::VariantChoice::Default => Self::Inherit,
            super::VariantChoice::Clear => Self::Off,
            super::VariantChoice::Exact(name) => Self::Named(name),
        }
    }
}

/// A model replacement resets `variant` to `Inherit` unless explicitly supplied.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    pub model: Option<String>,
    pub variant: AgentVariantOverride,
}

pub type AgentOverrides = std::collections::BTreeMap<String, AgentOverride>;

/// Fully applied selection, not an admitted or pending switch.
#[derive(Debug, Clone)]
pub struct EffectiveAgentSelection {
    pub provenance: Option<SelectionProvenance>,
    pub model: String,
    pub resolution: crate::provider::catalog::ModelResolution,
    pub variant: Option<(String, crate::provider::config::Variant)>,
    pub primary_agent: String,
    pub reasoning_floor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Config,
    Header,
    DefaultAgent,
    Parent,
    Override,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantSource {
    Config,
    Header,
    DefaultAgent,
    Parent,
    Override,
    ModelDefault,
    LayerPreference,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionProvenance {
    pub model: ModelSource,
    pub variant: VariantSource,
}

/// Staged compaction installed only if its selection transaction succeeds.
#[derive(Debug, Clone)]
pub struct SelectionCheckpoint {
    pub transcript: Vec<ChatMessage>,
    pub checkpoint: Compaction,
}

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
    /// A non-null reason makes this an explicitly marked read-only archive.
    pub resume_disabled_reason: Option<String>,
    pub provenance: Option<SelectionProvenance>,
    pub agent_overrides: AgentOverrides,
    pub id: Ulid,
    pub workspace: String,
    pub model: String,
    pub editor: bool,
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

impl StoredSession {
    /// Read-only is derived only from the explicit archive marker, not missing resolution.
    pub fn read_only(&self) -> bool {
        self.resume_disabled_reason.is_some()
    }
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

struct ChildLinkRow {
    parent_id: String,
    child_id: String,
    agent: String,
    goal: String,
    policy_snapshot: String,
    status: String,
    created_at: String,
    terminal_at: Option<String>,
    terminal_reason: Option<String>,
    result_text: Option<String>,
}

fn child_link_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChildLinkRow> {
    Ok(ChildLinkRow {
        parent_id: row.get(0)?,
        child_id: row.get(1)?,
        agent: row.get(2)?,
        goal: row.get(3)?,
        policy_snapshot: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        terminal_at: row.get(7)?,
        terminal_reason: row.get(8)?,
        result_text: row.get(9)?,
    })
}

impl ChildLinkRow {
    fn decode(self) -> Result<ChildLink, String> {
        Ok(ChildLink {
            parent_id: self
                .parent_id
                .parse()
                .map_err(|e| format!("parent id: {e}"))?,
            child_id: self
                .child_id
                .parse()
                .map_err(|e| format!("child id: {e}"))?,
            agent: self.agent,
            goal: self.goal,
            policy_snapshot: self.policy_snapshot,
            status: self.status,
            created_at: self.created_at,
            terminal_at: self.terminal_at,
            terminal_reason: self.terminal_reason,
            result_text: self.result_text,
        })
    }
}

/// The result channel of an acknowledged write.
type Ack = oneshot::Sender<Result<(), String>>;

/// What the writer thread accepts.
enum Write {
    WorkspaceVisibility {
        path: String,
        hidden: bool,
    },
    MarkArchive {
        id: Ulid,
        reason: String,
    },
    AgentSelection {
        #[cfg(test)]
        before_commit: Option<(
            std::sync::Arc<std::sync::Barrier>,
            std::sync::Arc<std::sync::Barrier>,
        )>,
        cancel: Option<tokio_util::sync::CancellationToken>,
        id: Ulid,
        agent: String,
        value: Option<AgentOverride>,
        selection: Option<Box<EffectiveAgentSelection>>,
        staged: Option<SelectionCheckpoint>,
        envelopes: Arc<Vec<EventEnvelope>>,
    },
    RecordSession {
        provenance: Option<String>,
        overrides: String,
        id: Ulid,
        workspace: String,
        model: String,
        editor: bool,
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
        envelope: Arc<EventEnvelope>,
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
    #[error("invalid agent selection: {0}")]
    InvalidSelection(String),
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serialize: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no stored session {0}")]
    UnknownSession(Ulid),
    #[error("the saved model resolution of session {0} changed while it was being completed")]
    ResolutionChanged(Ulid),
    #[error("conflicting event for session {id} at seq {seq}")]
    EventConflict { id: Ulid, seq: u64 },
}

/// The handle side of the store: clones a sender into the single writer
/// thread plus the shared path for blocking reads. Cheap to clone.
#[derive(Clone)]
pub struct HistoryStore {
    owner: Option<Ulid>,
    tx: mpsc::Sender<Msg>,
    path: Arc<PathBuf>,
    #[cfg(debug_assertions)]
    selection_commit_barrier: Arc<std::sync::Mutex<Option<Arc<SelectionCommitBarrier>>>>,
}

/// Debug-only boundary: SQLite has committed, but the selection callback has
/// not returned to install or publish anything. Scoped to this store instance.
#[cfg(debug_assertions)]
#[derive(Debug, Default)]
pub struct SelectionCommitBarrier {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    cancel: std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
}

#[cfg(debug_assertions)]
impl SelectionCommitBarrier {
    pub async fn cancelled(&self) {
        let cancel = self.cancel.lock().unwrap().clone().expect("commit reached");
        cancel.cancelled().await;
    }
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
    #[cfg(debug_assertions)]
    pub fn arm_selection_commit_barrier(&self) -> Arc<SelectionCommitBarrier> {
        let barrier = Arc::new(SelectionCommitBarrier::default());
        *self.selection_commit_barrier.lock().unwrap() = Some(barrier.clone());
        barrier
    }

    /// Updates only the saved override. Runtime applications should use
    /// `commit_agent_selection` inside the existing sequencer acknowledged callback.
    pub async fn update_agent_override(
        &self,
        id: Ulid,
        agent: &str,
        value: AgentOverride,
    ) -> Result<(), String> {
        self.acked(Write::AgentSelection {
            #[cfg(test)]
            before_commit: None,
            cancel: None,
            id,
            agent: agent.into(),
            value: Some(value),
            selection: None,
            staged: None,
            envelopes: Arc::new(vec![]),
        })
        .await
    }

    pub async fn clear_agent_override(&self, id: Ulid, agent: &str) -> Result<(), String> {
        self.acked(Write::AgentSelection {
            #[cfg(test)]
            before_commit: None,
            cancel: None,
            id,
            agent: agent.into(),
            value: None,
            selection: None,
            staged: None,
            envelopes: Arc::new(vec![]),
        })
        .await
    }

    /// Override, effective identity and supplied sequenced events commit together.
    /// `None` removes this agent's override; other agents are never changed.
    /// Call only on successful application, from the sequencer commit callback,
    /// before installing the in-memory selection or publishing its events.
    pub async fn commit_agent_selection(
        &self,
        id: Ulid,
        agent: &str,
        value: Option<AgentOverride>,
        selection: EffectiveAgentSelection,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<(), String> {
        self.commit_agent_selection_with_checkpoint(id, agent, value, selection, None, envelopes)
            .await
    }

    /// Atomically commits selection, optional staged transcript/checkpoint, and events.
    /// With `None`, the existing transcript and checkpoint are preserved.
    pub async fn commit_agent_selection_with_checkpoint(
        &self,
        id: Ulid,
        agent: &str,
        value: Option<AgentOverride>,
        selection: EffectiveAgentSelection,
        staged: Option<SelectionCheckpoint>,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<(), String> {
        self.commit_agent_selection_cancellable(
            id, agent, value, selection, staged, envelopes, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn commit_agent_selection_cancellable(
        &self,
        id: Ulid,
        agent: &str,
        value: Option<AgentOverride>,
        selection: EffectiveAgentSelection,
        staged: Option<SelectionCheckpoint>,
        envelopes: impl Into<Arc<Vec<EventEnvelope>>>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(), String> {
        let envelopes = envelopes.into();
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err("selection cancelled before admission".into());
        }
        if agent != selection.primary_agent {
            return Err(format!(
                "override agent '{agent}' does not match selection primary_agent '{}'",
                selection.primary_agent
            ));
        }
        if envelopes.is_empty()
            || envelopes.iter().any(|e| {
                e.session_id != id
                    || !matches!(
                        e.kind.as_str(),
                        "model.changed" | "session.agent_changed" | "context.compacted"
                    )
            })
        {
            return Err(
                "agent selection requires same-session model.changed/session.agent_changed events"
                    .into(),
            );
        }
        #[cfg(debug_assertions)]
        let token = cancel.clone();
        self.acked(Write::AgentSelection {
            #[cfg(test)]
            before_commit: None,
            cancel,
            id,
            agent: agent.into(),
            value,
            selection: Some(Box::new(selection)),
            staged,
            envelopes,
        })
        .await?;
        #[cfg(debug_assertions)]
        {
            let barrier = self.selection_commit_barrier.lock().unwrap().take();
            if let Some(barrier) = barrier {
                *barrier.cancel.lock().unwrap() = token;
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        Ok(())
    }
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
            owner: None,
            tx,
            path: Arc::new(path),
            #[cfg(debug_assertions)]
            selection_commit_barrier: Arc::new(std::sync::Mutex::new(None)),
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
        envelope: impl Into<Arc<EventEnvelope>>,
    ) -> Result<(), String> {
        let envelope = envelope.into();
        let path = self.path.clone();
        let owner = self.owner.map(|id| id.to_string());
        tokio::task::spawn_blocking(move || {
            let parent_id = envelope.session_id;
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            let tx = immediate_tx(&mut conn)?;
            let inserted = tx.execute(
                "INSERT INTO child_links (parent_id, child_id, agent, goal, policy_snapshot, status, created_at, owner_instance, delivery)
                 SELECT ?1, ?2, ?3, ?4, ?5, 'running', ?6, ?8, ?9
                 WHERE (SELECT COUNT(*) FROM child_links WHERE parent_id=?1 AND status IN ('starting','running')) < ?7
                   AND EXISTS(SELECT 1 FROM sessions WHERE id=?1)
                   AND EXISTS(SELECT 1 FROM sessions WHERE id=?2)",
                params![parent_id.to_string(), child_id.to_string(), agent, goal,
                    policy_snapshot, now_rfc3339(), capacity as i64, owner, envelope.payload["delivery"].as_str().unwrap_or("next_turn")],
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

    pub async fn child_ids(&self, parent: Ulid) -> Result<Vec<Ulid>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare("SELECT child_id FROM child_links WHERE parent_id=?1 ORDER BY created_at, child_id").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([parent.to_string()], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rows.map(|row| row.map_err(|e| e.to_string())?.parse().map_err(|e| format!("child id: {e}"))).collect()
        }).await.map_err(|e| e.to_string())?
    }

    /// Persists visibility for a canonical workspace path.
    pub async fn set_workspace_hidden(&self, path: &Path, hidden: bool) -> Result<(), String> {
        self.acked(Write::WorkspaceVisibility {
            path: path.to_string_lossy().into_owned(),
            hidden,
        })
        .await
    }

    /// Canonical paths hidden by the Captain, shared by every server using this history.
    pub async fn hidden_workspaces(&self) -> Result<std::collections::HashSet<PathBuf>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn
                .prepare("SELECT path FROM hidden_workspaces")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.map(|row| row.map(PathBuf::from).map_err(|e| e.to_string()))
                .collect()
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Root sessions matching a canonical directory, including alternate stored spellings.
    pub async fn workspace_session_ids(&self, workspace: &Path) -> Result<Vec<Ulid>, String> {
        let path = self.path.clone();
        let workspace = workspace.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare("SELECT id, workspace FROM sessions s WHERE NOT EXISTS (SELECT 1 FROM child_links c WHERE c.child_id=s.id)").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            let mut ids = Vec::new();
            for row in rows {
                let (id, stored) = row.map_err(|e| e.to_string())?;
                if std::fs::canonicalize(stored).ok().as_ref() == Some(&workspace) {
                    ids.push(id.parse().map_err(|e| format!("session id: {e}"))?);
                }
            }
            Ok(ids)
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
            finish_child_link_sync(&path, child_id, status, reason, result)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Persists the claimed stop reason without freeing capacity or delivering output.
    pub async fn claim_watchdog_stop(&self, child: Ulid) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            let changed = conn.execute("UPDATE child_links SET terminal_reason='watchdog' WHERE child_id=?1 AND status IN ('starting','running')", [child.to_string()]).map_err(|e| e.to_string())?;
            if changed != 1 { return Err("child already terminal".into()); }
            Ok(())
        }).await.map_err(|e| e.to_string())?
    }

    pub(super) fn set_owner(&mut self, owner: Ulid) {
        self.owner = Some(owner);
    }

    pub(super) fn recover_children(&self, instances: &Path) -> Result<(), String> {
        let conn = Connection::open(&*self.path).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT child_id, owner_instance FROM child_links WHERE status IN ('starting','running')")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        drop(conn);
        let mut locks = std::collections::HashMap::new();
        for (id, owner) in ids {
            let owner = owner
                .map(|id| id.parse::<Ulid>().map_err(|e| e.to_string()))
                .transpose()?;
            if let Some(owner) = owner {
                if let std::collections::hash_map::Entry::Vacant(entry) = locks.entry(owner) {
                    let Some(lock) = super::instance::stale_lock(instances, owner)? else {
                        continue;
                    };
                    entry.insert(lock);
                }
            }
            finish_child_link_sync(
                &self.path,
                id.parse().map_err(|e| format!("child id: {e}"))?,
                "stopped".into(),
                Some("restart".into()),
                String::new(),
            )?;
        }
        for (owner, _lock) in locks {
            std::fs::remove_file(instances.join(format!("{owner}.lock")))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub async fn child_link(&self, child_id: Ulid) -> Result<Option<ChildLink>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.query_row(
                "SELECT parent_id, child_id, agent, goal, policy_snapshot, status, created_at, terminal_at, terminal_reason, result_text
                 FROM child_links WHERE child_id=?1", [child_id.to_string()], child_link_from_row
            ).optional().map_err(|e| e.to_string())?.map(ChildLinkRow::decode).transpose()
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

    pub async fn repair_permission_mirrors(&self, parent: Ulid) -> Result<(), String> {
        let events = self.permission_events(parent).await?;
        let mut open = std::collections::BTreeMap::new();
        for event in events {
            if event.kind == "permission.requested" {
                if let Some(child) = event.payload["child_session_id"].as_str() {
                    open.insert(
                        event.payload["request_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        (child.to_string(), event.payload),
                    );
                }
            } else {
                open.remove(event.payload["request_id"].as_str().unwrap_or_default());
            }
        }
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            let tx = immediate_tx(&mut conn)?;
            for (request, (child, payload)) in open {
                let terminal = tx.query_row("SELECT status FROM child_links WHERE child_id=?1", [&child], |row| row.get::<_, String>(0)).optional().map_err(|e| e.to_string())?;
                let resolution = tx.query_row("SELECT envelope FROM events WHERE session_id=?1 AND kind='permission.resolved' AND json_extract(envelope,'$.payload.request_id')=?2 ORDER BY seq DESC LIMIT 1", params![child, request], |row| row.get::<_, String>(0)).optional().map_err(|e| e.to_string())?;
                let mut resolved = if let Some(event) = resolution {
                    serde_json::from_str::<EventEnvelope>(&event).map_err(|e| e.to_string())?.payload
                } else if terminal.as_deref().is_none_or(|state| !matches!(state,"running"|"starting")) {
                    serde_json::json!({"request_id":request,"tool_call_id":payload["tool_call_id"],"resolution":"expired"})
                } else { continue; };
                resolved["child_session_id"] = serde_json::json!(child);
                queue_permission_resolution(&tx, parent, &resolved).map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn pending_permission_mirrors(
        &self,
        parent: Ulid,
    ) -> Result<Vec<(String, serde_json::Value)>, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare("SELECT request_id,payload FROM permission_outbox WHERE parent_id=?1 AND delivered=0 ORDER BY rowid").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([parent.to_string()], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            rows.map(|row| { let (id,payload) = row.map_err(|e| e.to_string())?; Ok((id,serde_json::from_str(&payload).map_err(|e| e.to_string())?)) }).collect()
        }).await.map_err(|e| e.to_string())?
    }

    pub async fn drain_permission_mirrors(
        &self,
        parent: Ulid,
        sequencer: crate::bus::sequencer::Sequencer,
    ) -> Result<(), String> {
        self.repair_permission_mirrors(parent).await?;
        for (id, payload) in self.pending_permission_mirrors(parent).await? {
            let path = self.path.clone();
            let result = sequencer.commit_acked(crate::bus::sequencer::Candidate::new(EventKind::PermissionResolved,payload), Box::new(move |event| Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
                    conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
                    let tx = immediate_tx(&mut conn)?;
                    let changed = tx.execute("UPDATE permission_outbox SET delivered=1 WHERE parent_id=?1 AND request_id=?2 AND delivered=0",params![parent.to_string(),id]).map_err(|e| e.to_string())?;
                    if changed == 0 { return Err("permission mirror already delivered".into()); }
                    insert_committed_event(&tx,&event)?;
                    tx.commit().map_err(|e| e.to_string())
                }).await.map_err(|e| e.to_string())?
            }))).await;
            if let Err(error) = result {
                if error != "permission mirror already delivered" {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub async fn deliver_completion(
        &self,
        child: Ulid,
        envelope: impl Into<Arc<EventEnvelope>>,
    ) -> Result<(), String> {
        let envelope = envelope.into();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            let tx = immediate_tx(&mut conn)?;
            let changed = tx.execute("UPDATE completion_outbox SET delivered=1 WHERE child_id=?1 AND parent_id=?2 AND delivered=0", params![child.to_string(), envelope.session_id.to_string()]).map_err(|e| e.to_string())?;
            if changed == 0 { return Err("completion already delivered".into()); }
            insert_committed_event(&tx, &envelope)?;
            tx.commit().map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
    }

    /// Uses the same durable outbox and sequencer commit as child completion.
    pub async fn enqueue_watchdog(
        &self,
        parent: Ulid,
        payload: serde_json::Value,
        result: String,
        consume: bool,
    ) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            conn.execute("INSERT INTO completion_outbox (child_id,parent_id,outcome,result_ref,payload,result_text,consumed) VALUES (?1,?2,'watchdog',?1,?3,?4,?5)",
                params![Ulid::new().to_string(), parent.to_string(), payload.to_string(), result, !consume]).map_err(|e| e.to_string())?;
            Ok(())
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
        self.consume_completions_committed(parent, children, transcript, None)
            .await
    }

    /// Transcript, consumption markers and the sequenced message commit together.
    pub async fn consume_completions_committed(
        &self,
        parent: Ulid,
        children: Vec<Ulid>,
        transcript: Vec<ChatMessage>,
        envelope: Option<EventEnvelope>,
    ) -> Result<(), String> {
        self.consume_completions_shared(parent, children, transcript, envelope.map(Arc::new))
            .await
    }

    pub(crate) async fn consume_completions_shared(
        &self,
        parent: Ulid,
        children: Vec<Ulid>,
        transcript: Vec<ChatMessage>,
        envelope: Option<Arc<EventEnvelope>>,
    ) -> Result<(), String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&*path).map_err(|e| e.to_string())?;
            conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|e| e.to_string())?;
            let tx = immediate_tx(&mut conn)?;
            for child in children {
                let changed = tx.execute("UPDATE completion_outbox SET consumed=1 WHERE child_id=?1 AND parent_id=?2 AND consumed=0", params![child.to_string(), parent.to_string()]).map_err(|e| e.to_string())?;
                if changed != 1 { return Err("completion already consumed or missing".into()); }
            }
            let changed = tx.execute("UPDATE sessions SET transcript=?2, updated_at=?3 WHERE id=?1", params![parent.to_string(), serde_json::to_string(&transcript).map_err(|e| e.to_string())?, now_rfc3339()]).map_err(|e| e.to_string())?;
            if changed != 1 { return Err("completion parent is missing".into()); }
            if let Some(envelope) = envelope { insert_committed_event(&tx, &envelope)?; }
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
        editor: bool,
    ) -> Result<(), String> {
        self.record_session_resolved(id, workspace, model, editor, None, None, None)
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
        editor: bool,
        resolution: Option<&crate::provider::catalog::ModelResolution>,
        variant: Option<&(String, crate::provider::config::Variant)>,
        primary_agent: Option<&str>,
    ) -> Result<(), String> {
        self.record_session_with_overrides(
            id,
            workspace,
            model,
            editor,
            resolution,
            variant,
            primary_agent,
            &AgentOverrides::new(),
        )
        .await
    }

    /// Initial identity and complete overrides are inserted in one writer transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_session_with_overrides(
        &self,
        id: Ulid,
        workspace: &str,
        model: &str,
        editor: bool,
        resolution: Option<&crate::provider::catalog::ModelResolution>,
        variant: Option<&(String, crate::provider::config::Variant)>,
        primary_agent: Option<&str>,
        overrides: &AgentOverrides,
    ) -> Result<(), String> {
        self.record_session_with_provenance(
            id,
            workspace,
            model,
            editor,
            resolution,
            variant,
            primary_agent,
            overrides,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_session_with_provenance(
        &self,
        id: Ulid,
        workspace: &str,
        model: &str,
        editor: bool,
        resolution: Option<&crate::provider::catalog::ModelResolution>,
        variant: Option<&(String, crate::provider::config::Variant)>,
        primary_agent: Option<&str>,
        overrides: &AgentOverrides,
        provenance: Option<&SelectionProvenance>,
    ) -> Result<(), String> {
        validate_agent_overrides(overrides)?;
        let (variant, variant_override) = split_variant(variant)?;
        let resolution = resolution.map(serialize_resolution).transpose()?;
        self.acked(Write::RecordSession {
            provenance: provenance
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| e.to_string())?,
            overrides: serde_json::to_string(overrides).map_err(|e| e.to_string())?,
            id,
            workspace: workspace.to_string(),
            model: model.to_string(),
            editor,
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
        let resolution = serialize_resolution(resolution)?;
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
        let resolution = serialize_resolution(resolution)?;
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
        let resolution = serialize_resolution(resolution)?;
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
        self.enqueue(Write::AppendEvent {
            id,
            envelope: Arc::new(envelope),
        });
    }

    /// Appends one durable envelope like [`HistoryStore::append_event`] and
    /// waits for ITS transaction: the result is that append's own outcome,
    /// not the store's sticky first failure.
    pub async fn append_event_acked(
        &self,
        id: Ulid,
        envelope: impl Into<Arc<EventEnvelope>>,
    ) -> Result<(), String> {
        self.acked(Write::AppendEvent {
            id,
            envelope: envelope.into(),
        })
        .await
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

    /// Sets the session title (first captain message; only when unset).
    /// Fire-and-forget, like [`HistoryStore::append_event`].
    pub fn set_title(&self, id: Ulid, title: String) {
        self.enqueue(Write::SetTitle { id, title });
    }

    /// Removes a session row and its events in one transaction, and waits
    /// for the commit.
    pub async fn delete(&self, id: Ulid) -> Result<(), String> {
        self.acked(Write::Delete { id }).await
    }

    /// Mark an existing historical session read-only without changing its history.
    /// OFFLINE ONLY: the caller must quiesce all runtimes using this database.
    /// This raw store cannot inspect registries; live callers use `Service::mark_archive`.
    /// Reasons are single-line safe text, at most 512 UTF-8 bytes.
    pub async fn mark_archive(&self, id: Ulid, reason: &str) -> Result<(), String> {
        if reason.trim().is_empty()
            || reason.len() > 512
            || reason.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        {
            return Err("archive reason must be nonempty safe single-line text of at most 512 bytes".into());
        }
        self.acked(Write::MarkArchive {
            id,
            reason: reason.to_string(),
        })
        .await
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

/// One row of the list projection: the session metadata plus its captain
/// message count (`message.received` events).
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub meta: StoredSession,
    pub message_count: u64,
}

/// One workspace seen in the history: how many conversations it has and when the
/// latest one changed. Subagent (child) sessions are not counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub workspace: String,
    pub session_count: u64,
    pub updated_at: String,
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
fn finish_child_link_sync(
    path: &Path,
    child_id: Ulid,
    status: String,
    reason: Option<String>,
    result: String,
) -> Result<Option<ChildLink>, String> {
    let outcome = match status.as_str() {
        "done" => crate::bus::event::SubagentOutcome::Done,
        "failed" => crate::bus::event::SubagentOutcome::Failed,
        "stopped" => crate::bus::event::SubagentOutcome::Stopped,
        _ => return Err(format!("invalid terminal child status: {status}")),
    };
    let mut conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.busy_timeout(WRITER_BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;
    let tx = immediate_tx(&mut conn)?;
    let terminal_at = now_rfc3339();
    let changed = tx
        .execute(
            "UPDATE child_links SET status=?2, terminal_at=?3, terminal_reason=?4, result_text=?5
                 WHERE child_id=?1 AND status IN ('starting','running')",
            params![child_id.to_string(), status, terminal_at, reason, result],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        tx.rollback().map_err(|e| e.to_string())?;
        return Ok(None);
    }
    let (row, delivery) = tx.query_row(
                "SELECT parent_id, child_id, agent, goal, policy_snapshot, status, created_at, terminal_at, terminal_reason, result_text, delivery
                 FROM child_links WHERE child_id=?1",
                [child_id.to_string()], |row| Ok((child_link_from_row(row)?, row.get::<_, String>(10)?)),
            ).map_err(|e| e.to_string())?;
    let mut payload = serde_json::to_value(&crate::bus::event::SubagentDone {
        parent_session_id: row
            .parent_id
            .parse()
            .map_err(|e| format!("parent id: {e}"))?,
        child_session_id: child_id,
        agent: row.agent.clone(),
        outcome,
        reason: reason.clone(),
        notification_id: Ulid::new(),
    })
    .map_err(|e| e.to_string())?;
    payload["delivery"] = serde_json::json!(delivery);
    let payload = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO completion_outbox (child_id, parent_id, outcome, result_ref, payload, result_text) VALUES (?1, ?2, ?3, ?1, ?4, ?5)",
                params![child_id.to_string(), row.parent_id, status, payload, result]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    row.decode().map(Some)
}

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

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let exists = conn
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name='{column}'"
        ))?
        .exists([])?;
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn immediate_tx(conn: &mut Connection) -> Result<rusqlite::Transaction<'_>, String> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())
}

fn serialize_resolution(
    resolution: &crate::provider::catalog::ModelResolution,
) -> Result<String, String> {
    serde_json::to_string(resolution).map_err(|e| format!("serialize model resolution: {e}"))
}

fn queue_permission_resolution(
    tx: &rusqlite::Transaction<'_>,
    parent: Ulid,
    payload: &serde_json::Value,
) -> Result<(), WriteError> {
    tx.execute(
        "INSERT OR IGNORE INTO permission_outbox (parent_id,request_id,payload) VALUES (?1,?2,?3)",
        params![
            parent.to_string(),
            payload["request_id"].as_str().unwrap_or_default(),
            serde_json::to_string(payload)?
        ],
    )?;
    Ok(())
}

fn apply(conn: &mut Connection, write: Write) -> Result<(), WriteError> {
    #[cfg(test)]
    let before_commit = match &write {
        Write::AgentSelection { before_commit, .. } => before_commit.clone(),
        _ => None,
    };
    let cancellation = match &write {
        Write::AgentSelection { cancel, .. } => cancel.clone(),
        _ => None,
    };
    if cancellation.as_ref().is_some_and(|c| c.is_cancelled()) {
        return Err(WriteError::InvalidSelection("selection cancelled".into()));
    }
    // Acquire write ownership before reads: a deferred WAL snapshot cannot
    // upgrade after an outbox connection commits, even with a busy timeout.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    match write {
        Write::AgentSelection {
            #[cfg(test)]
                before_commit: _,
            cancel: _,
            id,
            agent,
            value,
            selection,
            staged,
            envelopes,
        } => {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT agent_overrides FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(WriteError::UnknownSession(id))?;
            let mut overrides =
                decode_agent_overrides(raw.as_deref()).map_err(WriteError::InvalidSelection)?;
            if let Some(value) = value {
                overrides.insert(agent, value);
            } else {
                overrides.remove(&agent);
            }
            validate_agent_overrides(&overrides).map_err(WriteError::InvalidSelection)?;
            tx.execute(
                "UPDATE sessions SET agent_overrides=?2 WHERE id=?1",
                params![id.to_string(), serde_json::to_string(&overrides)?],
            )?;
            if let Some(selection) = selection {
                let (variant, variant_override) = split_variant(selection.variant.as_ref())
                    .map_err(WriteError::InvalidSelection)?;
                let floor = i64::try_from(selection.reasoning_floor)
                    .map_err(|e| WriteError::InvalidSelection(e.to_string()))?;
                tx.execute("UPDATE sessions SET model=?2, model_resolution=?3, variant=?4, variant_override=?5, primary_agent=?6, reasoning_floor=?7, selection_provenance=?8 WHERE id=?1", params![id.to_string(), selection.model, serde_json::to_string(&selection.resolution)?, variant, variant_override, selection.primary_agent, floor, selection.provenance.map(|p| serde_json::to_string(&p)).transpose()?])?;
            }
            if let Some(staged) = staged {
                crate::agent::compaction::validate(&staged.checkpoint, &staged.transcript)
                    .map_err(|e| WriteError::InvalidSelection(e.to_string()))?;
                tx.execute(
                    "UPDATE sessions SET transcript = ?2, checkpoint = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![
                        id.to_string(),
                        serde_json::to_string(&staged.transcript)?,
                        staged.checkpoint.to_json(),
                        now_rfc3339()
                    ],
                )?;
            }
            for event in envelopes.iter() {
                insert_committed_event(&tx, event).map_err(WriteError::InvalidSelection)?;
            }
        }
        Write::RecordSession {
            provenance,
            overrides,
            id,
            workspace,
            model,
            editor,
            resolution,
            variant,
            variant_override,
            primary_agent,
        } => {
            let now = now_rfc3339();
            tx.execute(
                "INSERT OR IGNORE INTO sessions
                 (id, workspace, model, editor, title, created_at, updated_at, last_seq, transcript,
                   model_resolution, variant, variant_override, primary_agent, agent_overrides, selection_provenance)
                  VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, -1, NULL, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id.to_string(),
                    workspace,
                    model,
                    editor,
                    now,
                    resolution,
                    variant,
                    variant_override,
                    primary_agent,
                    overrides,
                    provenance
                ],
            )?;
            if let Ok(path) = std::fs::canonicalize(&workspace) {
                tx.execute(
                    "DELETE FROM hidden_workspaces WHERE path=?1",
                    [path.to_string_lossy().as_ref()],
                )?;
            }
        }
        Write::WorkspaceVisibility { path, hidden } => {
            if hidden {
                tx.execute(
                    "INSERT OR REPLACE INTO hidden_workspaces (path, hidden_at) VALUES (?1, ?2)",
                    params![path, now_rfc3339()],
                )?;
            } else {
                tx.execute("DELETE FROM hidden_workspaces WHERE path=?1", [path])?;
            }
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
            if envelope.kind == "permission.resolved" {
                let parent = tx
                    .query_row(
                        "SELECT parent_id FROM child_links WHERE child_id=?1",
                        [id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(parent) = parent {
                    let mut payload = envelope.payload.clone();
                    payload["child_session_id"] = serde_json::json!(id);
                    let parent = parent.parse().map_err(|_| WriteError::UnknownSession(id))?;
                    queue_permission_resolution(&tx, parent, &payload)?;
                }
            }
            let payload = serde_json::to_string(envelope.as_ref())?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO events (session_id, seq, kind, envelope)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id.to_string(), envelope.seq as i64, envelope.kind, payload],
            )?;
            if inserted == 0 {
                let (kind, existing): (String, String) = tx.query_row(
                    "SELECT kind, envelope FROM events WHERE session_id=?1 AND seq=?2",
                    params![id.to_string(), envelope.seq as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                // Compare JSON values so formatting of an existing row is immaterial.
                if kind != envelope.kind
                    || serde_json::from_str::<serde_json::Value>(&existing)?
                        != serde_json::from_str::<serde_json::Value>(&payload)?
                {
                    return Err(WriteError::EventConflict {
                        id,
                        seq: envelope.seq,
                    });
                }
            }
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
                "DELETE FROM permission_outbox WHERE parent_id=?1",
                [id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM completion_outbox WHERE parent_id=?1",
                [id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM child_links WHERE child_id=?1 OR parent_id=?1",
                [id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM events WHERE session_id = ?1",
                params![id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![id.to_string()],
            )?;
        }
        Write::MarkArchive { id, reason } => {
            if tx.execute(
                "UPDATE sessions SET resume_disabled_reason = ?2 WHERE id = ?1",
                params![id.to_string(), reason],
            )? == 0
            {
                return Err(WriteError::UnknownSession(id));
            }
        }
    }
    #[cfg(test)]
    if let Some((reached, release)) = before_commit {
        reached.wait();
        release.wait();
    }
    if cancellation.as_ref().is_some_and(|c| c.is_cancelled()) {
        return Err(WriteError::InvalidSelection(
            "selection cancelled before commit".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

/// Creates the schema when missing. Idempotent.
fn migrate(path: &Path) -> Result<(), String> {
    let conn = Connection::open(path).map_err(|e| format!("open: {e}"))?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hidden_workspaces (
            path TEXT PRIMARY KEY,
            hidden_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            workspace TEXT NOT NULL,
            model TEXT NOT NULL,
            profile TEXT,
            editor INTEGER NOT NULL DEFAULT 0,
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
            reasoning_floor INTEGER,
            resume_disabled_reason TEXT
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
        CREATE TABLE IF NOT EXISTS permission_outbox (
            parent_id TEXT NOT NULL,
            request_id TEXT NOT NULL,
            payload TEXT NOT NULL,
            delivered INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(parent_id,request_id)
        );
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
    // Editor context is explicit since October 2026. Legacy profile values are
    // left untouched for manual migration, never interpreted on normal startup.
    add_column_if_missing(&conn, "sessions", "editor", "INTEGER NOT NULL DEFAULT 0")
        .map_err(|e| format!("migrate: {e}"))?;
    add_column_if_missing(&conn, "child_links", "owner_instance", "TEXT")
        .map_err(|e| e.to_string())?;
    add_column_if_missing(
        &conn,
        "child_links",
        "delivery",
        "TEXT NOT NULL DEFAULT 'next_turn'",
    )
    .map_err(|e| e.to_string())?;
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
    add_column_if_missing(&conn, "sessions", "model_resolution", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    // A database from before `variant`: add the nullable column.
    add_column_if_missing(&conn, "sessions", "variant", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    // A database from before `variant_override`: add the nullable column.
    add_column_if_missing(&conn, "sessions", "variant_override", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    // A database from before `checkpoint`: add the nullable column.
    add_column_if_missing(&conn, "sessions", "checkpoint", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    // A database from before `primary_agent`: add the nullable column.
    add_column_if_missing(&conn, "sessions", "primary_agent", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    // A database from before `reasoning_floor`: add the nullable column.
    add_column_if_missing(&conn, "sessions", "reasoning_floor", "INTEGER")
        .map_err(|e| format!("migrate: {e}"))?;
    add_column_if_missing(&conn, "sessions", "agent_overrides", "TEXT")
        .map_err(|e| e.to_string())?;
    add_column_if_missing(&conn, "sessions", "selection_provenance", "TEXT")
        .map_err(|e| e.to_string())?;
    add_column_if_missing(&conn, "sessions", "resume_disabled_reason", "TEXT")
        .map_err(|e| format!("migrate: {e}"))?;
    Ok(())
}

fn decode_agent_overrides(raw: Option<&str>) -> Result<AgentOverrides, String> {
    let overrides = raw
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| format!("saved agent_overrides cannot decode: {e}"))?
        .unwrap_or_default();
    validate_agent_overrides(&overrides)?;
    Ok(overrides)
}

fn validate_agent_overrides(overrides: &AgentOverrides) -> Result<(), String> {
    let valid = |s: &str| !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control());
    for (agent, value) in overrides {
        if !valid(agent)
            || value.model.as_deref().is_some_and(|s| !valid(s))
            || matches!(&value.variant, AgentVariantOverride::Named(name) if !valid(name))
        {
            return Err(format!(
                "saved agent_overrides for agent '{agent}' contains an invalid name"
            ));
        }
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
    mirror: Option<(Ulid, Ulid, crate::bus::sequencer::Sequencer)>,
    bus: crate::bus::SessionBus,
    session_id: Ulid,
    store: HistoryStore,
}

impl PersistingEmitter {
    pub fn with_permission_mirror(
        mut self,
        child: Ulid,
        parent_id: Ulid,
        parent: crate::bus::sequencer::Sequencer,
    ) -> Self {
        self.mirror = Some((child, parent_id, parent));
        self
    }
    pub fn new(session_id: Ulid, bus: crate::bus::SessionBus, store: HistoryStore) -> Self {
        Self {
            mirror: None,
            bus,
            session_id,
            store,
        }
    }
}

impl Emitter for PersistingEmitter {
    fn emit(&mut self, envelope: EventEnvelope) {
        if matches!(
            envelope.kind.as_str(),
            "permission.requested" | "permission.resolved"
        ) {
            if let Some((child, _, parent)) = &self.mirror {
                let mut payload = envelope.payload.clone();
                payload["child_session_id"] = serde_json::json!(child);
                if envelope.kind == "permission.requested" {
                    let kind = EventKind::PermissionRequested;
                    let _ = parent.publish(crate::bus::sequencer::Candidate::new(kind, payload));
                }
            }
        }
        let kind = envelope.kind.as_str();
        // An unknown name is stored: only a catalog kind marked live-only is not.
        if EventKind::from_wire(kind).is_none_or(EventKind::is_durable) {
            // The first captain message becomes the title (the store guards:
            // only when the title is still NULL).
            if kind == "message.received" && envelope.payload["source"] != "subagent" {
                if let Some(content) = envelope.payload["content"].as_str() {
                    self.store
                        .set_title(self.session_id, title_from_message(content));
                }
            }
            self.store.append_event(self.session_id, envelope.clone());
            if envelope.kind == "permission.resolved" {
                if let Some((_, parent_id, parent)) = &self.mirror {
                    let (store, parent_id, parent) =
                        (self.store.clone(), *parent_id, parent.clone());
                    tokio::spawn(async move {
                        if store.flush().await.is_ok() {
                            let _ = store.drain_permission_mirrors(parent_id, parent).await;
                        }
                    });
                }
            }
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

/// Derives the session title from the first captain message: any trailing
/// `<editor-context>…</editor-context>` block removed, whitespace collapsed,
/// cut at 60 characters.
pub fn title_from_message(content: &str) -> String {
    let stripped = strip_editor_context(content);
    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(60).collect()
}

/// Removes a TRAILING `<editor-context>…</editor-context>` block (the editor
/// appends it after the Captain's text). Non-trailing blocks are kept.
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_at_writer_precommit_rolls_back_override_and_inserted_event() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "old", false).await.unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let reached = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let task = tokio::spawn({
            let store = store.clone();
            let token = token.clone();
            let reached = reached.clone();
            let release = release.clone();
            async move {
                store
                    .acked(Write::AgentSelection {
                        before_commit: Some((reached, release)),
                        cancel: Some(token),
                        id,
                        agent: "build".into(),
                        value: Some(AgentOverride {
                            model: Some("p/new".into()),
                            variant: AgentVariantOverride::Off,
                        }),
                        selection: None,
                        staged: None,
                        envelopes: Arc::new(vec![EventEnvelope::new(
                            id,
                            0,
                            EventKind::ModelChanged,
                            serde_json::json!({}),
                        )]),
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || reached.wait())
            .await
            .unwrap();
        token.cancel();
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        assert!(task
            .await
            .unwrap()
            .unwrap_err()
            .contains("cancelled before commit"));
        let loaded = store.load(id).await.unwrap().unwrap();
        assert!(loaded.meta.agent_overrides.is_empty());
        assert!(loaded.envelopes.is_empty());
        assert_eq!(loaded.meta.model, "old");
    }

    #[tokio::test]
    async fn cancelled_selection_writer_leaves_identity_override_and_events_unchanged() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "old", false).await.unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        // Exercise writer cancellation independently of the public admission check.
        let error = store
            .acked(Write::AgentSelection {
                before_commit: None,
                id,
                agent: "build".into(),
                value: Some(AgentOverride {
                    model: Some("p/new".into()),
                    variant: AgentVariantOverride::Off,
                }),
                selection: None,
                staged: None,
                envelopes: Arc::new(vec![EventEnvelope::new(
                    id,
                    0,
                    EventKind::ModelChanged,
                    serde_json::json!({}),
                )]),
                cancel: Some(token),
            })
            .await
            .unwrap_err();
        assert!(error.contains("cancelled"));
        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.meta.model, "old");
        assert!(loaded.meta.agent_overrides.is_empty());
        assert!(loaded.envelopes.is_empty());
    }

    #[tokio::test]
    async fn initial_overrides_and_identity_insert_atomically_or_leave_no_row() {
        let (dir, store) = temp_store();
        let mut overrides = AgentOverrides::new();
        overrides.insert(
            "build".into(),
            AgentOverride {
                model: Some("p/model".into()),
                variant: AgentVariantOverride::Off,
            },
        );
        let id = Ulid::new();
        store
            .record_session_with_overrides(id, "/w", "p/model", false, None, None, None, &overrides)
            .await
            .unwrap();
        assert_eq!(
            store.meta(id).await.unwrap().unwrap().agent_overrides,
            overrides
        );
        side_connection(&dir).execute_batch("CREATE TRIGGER fail_initial AFTER INSERT ON sessions BEGIN SELECT RAISE(ABORT, 'insert fault'); END;").unwrap();
        let failed = Ulid::new();
        assert!(store
            .record_session_with_overrides(
                failed, "/w", "p/model", false, None, None, None, &overrides
            )
            .await
            .unwrap_err()
            .contains("insert fault"));
        assert!(store.meta(failed).await.unwrap().is_none());
        assert!(store
            .replay(failed, None, u64::MAX)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn agent_overrides_are_independent_durable_clearable_and_deleted() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "old", false).await.unwrap();
        assert!(store
            .meta(id)
            .await
            .unwrap()
            .unwrap()
            .agent_overrides
            .is_empty());
        let build = AgentOverride {
            model: Some("p/build".into()),
            variant: AgentVariantOverride::Off,
        };
        let plan = AgentOverride {
            model: Some("p/plan".into()),
            variant: AgentVariantOverride::Named("low".into()),
        };
        store
            .update_agent_override(id, "build", build.clone())
            .await
            .unwrap();
        store
            .update_agent_override(id, "plan", plan.clone())
            .await
            .unwrap();
        store.set_primary_agent(id, Some("plan")).await.unwrap();
        store.set_primary_agent(id, Some("build")).await.unwrap();
        let reopened = HistoryStore::open(dir.path().join("sessions.db")).unwrap();
        let overrides = reopened.agent_overrides(id).await.unwrap().unwrap();
        assert_eq!(overrides["build"], build);
        assert_eq!(overrides["plan"], plan);
        let reset = AgentOverride {
            model: Some("p/new".into()),
            variant: super::super::VariantChoice::Default.into(),
        };
        reopened
            .update_agent_override(id, "build", reset.clone())
            .await
            .unwrap();
        assert_eq!(
            reopened.agent_overrides(id).await.unwrap().unwrap()["build"],
            reset
        );
        assert_ne!(reset.variant, build.variant);
        reopened.clear_agent_override(id, "plan").await.unwrap();
        assert!(!reopened
            .agent_overrides(id)
            .await
            .unwrap()
            .unwrap()
            .contains_key("plan"));
        reopened.delete(id).await.unwrap();
        assert!(reopened.agent_overrides(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn agent_selection_events_identity_and_overrides_commit_or_rollback_together() {
        use crate::provider::catalog::{Bounds, ModelResolution, Provenance, RESOLUTION_VERSION};
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "old", false).await.unwrap();
        let selection = EffectiveAgentSelection {
            provenance: None,
            model: "p/new".into(),
            primary_agent: "plan".into(),
            variant: Some(("low".into(), Default::default())),
            reasoning_floor: 4,
            resolution: ModelResolution {
                version: RESOLUTION_VERSION,
                provider: "p".into(),
                model: "new".into(),
                api: "chat".into(),
                endpoint: "http://h/v1".into(),
                history_policy: "preserve".into(),
                bounds: Bounds {
                    context_window: 100,
                    max_input_tokens: None,
                    reported_output: None,
                },
                provenance: Provenance {
                    context_window: "models.json".into(),
                    max_input_tokens: "none".into(),
                    reported_output: "none".into(),
                },
            },
        };
        let value = AgentOverride {
            model: Some("p/new".into()),
            variant: AgentVariantOverride::Inherit,
        };
        let events = vec![
            EventEnvelope::new(id, 0, EventKind::ModelChanged, serde_json::json!({})),
            EventEnvelope::new(id, 1, EventKind::SessionAgentChanged, serde_json::json!({})),
        ];
        let staged = SelectionCheckpoint {
            transcript: (0..4).flat_map(exchange).collect(),
            checkpoint: compaction(3, vec![6]),
        };
        store
            .commit_agent_selection_with_checkpoint(
                id,
                "plan",
                Some(value.clone()),
                selection.clone(),
                Some(staged.clone()),
                events.clone(),
            )
            .await
            .unwrap();
        let before = store.load(id).await.unwrap().unwrap();
        assert_eq!(before.meta.model, "p/new");
        assert_eq!(before.meta.variant.as_deref(), Some("low"));
        assert_eq!(before.meta.primary_agent.as_deref(), Some("plan"));
        assert_eq!(before.meta.reasoning_floor, Some(4));
        assert_eq!(before.meta.agent_overrides["plan"], value);
        assert_eq!(before.envelopes.len(), 2);
        assert_eq!(before.compaction, Some(staged.checkpoint.clone()));
        assert_eq!(
            serde_json::to_value(&before.transcript).unwrap(),
            serde_json::to_value(&staged.transcript).unwrap()
        );
        let replacement = SelectionCheckpoint {
            transcript: (0..5).flat_map(exchange).collect(),
            checkpoint: compaction(7, vec![10]),
        };
        let invalid = SelectionCheckpoint {
            transcript: vec![],
            checkpoint: compaction(99, vec![]),
        };
        let mut invalid_selection = selection.clone();
        invalid_selection.model = "p/invalid".into();
        invalid_selection.resolution.model = "invalid".into();
        invalid_selection.variant = None;
        invalid_selection.reasoning_floor = 99;
        assert!(store
            .commit_agent_selection_with_checkpoint(
                id,
                "plan",
                None,
                invalid_selection,
                Some(invalid),
                vec![EventEnvelope::new(
                    id,
                    2,
                    EventKind::ModelChanged,
                    serde_json::json!({})
                )],
            )
            .await
            .is_err());
        let invalid_after = store.load(id).await.unwrap().unwrap();
        assert_eq!(
            invalid_after.meta.agent_overrides,
            before.meta.agent_overrides
        );
        assert_eq!(invalid_after.meta.last_seq, before.meta.last_seq);
        assert_eq!(invalid_after.meta.model, before.meta.model);
        assert_eq!(
            invalid_after.meta.model_resolution,
            before.meta.model_resolution
        );
        assert_eq!(invalid_after.meta.variant, before.meta.variant);
        assert_eq!(
            invalid_after.meta.variant_override,
            before.meta.variant_override
        );
        assert_eq!(invalid_after.meta.primary_agent, before.meta.primary_agent);
        assert_eq!(
            invalid_after.meta.reasoning_floor,
            before.meta.reasoning_floor
        );
        assert_eq!(
            serde_json::to_value(&invalid_after.envelopes).unwrap(),
            serde_json::to_value(&before.envelopes).unwrap()
        );
        assert_eq!(invalid_after.compaction, before.compaction);
        assert_eq!(
            serde_json::to_value(&invalid_after.transcript).unwrap(),
            serde_json::to_value(&before.transcript).unwrap()
        );
        let mut mismatched = selection.clone();
        mismatched.model = "p/mismatched".into();
        mismatched.resolution.model = "mismatched".into();
        mismatched.primary_agent = "build".into();
        mismatched.variant = None;
        mismatched.reasoning_floor = 99;
        let error = store
            .commit_agent_selection_with_checkpoint(
                id,
                "plan",
                None,
                mismatched,
                Some(replacement.clone()),
                vec![
                    EventEnvelope::new(id, 2, EventKind::ModelChanged, serde_json::json!({})),
                    EventEnvelope::new(
                        id,
                        3,
                        EventKind::SessionAgentChanged,
                        serde_json::json!({}),
                    ),
                ],
            )
            .await
            .unwrap_err();
        assert!(error.contains("does not match"));
        let unchanged = store.load(id).await.unwrap().unwrap();
        assert_eq!(unchanged.meta.agent_overrides, before.meta.agent_overrides);
        assert_eq!(unchanged.meta.model, before.meta.model);
        assert_eq!(
            unchanged.meta.model_resolution,
            before.meta.model_resolution
        );
        assert_eq!(unchanged.meta.variant, before.meta.variant);
        assert_eq!(
            unchanged.meta.variant_override,
            before.meta.variant_override
        );
        assert_eq!(unchanged.meta.primary_agent, before.meta.primary_agent);
        assert_eq!(unchanged.meta.reasoning_floor, before.meta.reasoning_floor);
        assert_eq!(unchanged.meta.last_seq, before.meta.last_seq);
        assert_eq!(unchanged.compaction, before.compaction);
        assert_eq!(
            serde_json::to_value(&unchanged.transcript).unwrap(),
            serde_json::to_value(&before.transcript).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&unchanged.envelopes).unwrap(),
            serde_json::to_value(&before.envelopes).unwrap()
        );
        let mut failed = selection.clone();
        failed.model = "p/failed".into();
        failed.variant = None;
        failed.resolution.model = "failed".into();
        let failing_events = vec![
            EventEnvelope::new(id, 2, EventKind::ModelChanged, serde_json::json!({})),
            events[1].clone(),
        ];
        assert!(store
            .commit_agent_selection_with_checkpoint(
                id,
                "plan",
                None,
                failed,
                Some(replacement),
                failing_events
            )
            .await
            .is_err());
        let after = store.load(id).await.unwrap().unwrap();
        assert_eq!(after.meta.model, before.meta.model);
        assert_eq!(after.meta.model_resolution, before.meta.model_resolution);
        assert_eq!(after.meta.variant, before.meta.variant);
        assert_eq!(after.meta.variant_override, before.meta.variant_override);
        assert_eq!(after.meta.primary_agent, before.meta.primary_agent);
        assert_eq!(after.meta.reasoning_floor, before.meta.reasoning_floor);
        assert_eq!(after.meta.last_seq, before.meta.last_seq);
        assert_eq!(after.meta.agent_overrides, before.meta.agent_overrides);
        assert_eq!(after.envelopes.len(), 2);
        assert_eq!(after.compaction, before.compaction);
        assert_eq!(
            serde_json::to_value(&after.transcript).unwrap(),
            serde_json::to_value(&before.transcript).unwrap()
        );
        store
            .commit_agent_selection(
                id,
                "plan",
                Some(value),
                selection.clone(),
                vec![
                    EventEnvelope::new(id, 10, EventKind::ModelChanged, serde_json::json!({})),
                    EventEnvelope::new(
                        id,
                        4,
                        EventKind::SessionAgentChanged,
                        serde_json::json!({}),
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.meta(id).await.unwrap().unwrap().last_seq, 10);
        // A later transaction containing only older sequences must not lower the cursor.
        store
            .commit_agent_selection(
                id,
                "plan",
                None,
                selection,
                vec![EventEnvelope::new(
                    id,
                    5,
                    EventKind::ModelChanged,
                    serde_json::json!({}),
                )],
            )
            .await
            .unwrap();
        assert_eq!(store.meta(id).await.unwrap().unwrap().last_seq, 10);
        let preserved = store.load(id).await.unwrap().unwrap();
        assert_eq!(preserved.compaction, before.compaction);
        assert_eq!(
            serde_json::to_value(&preserved.transcript).unwrap(),
            serde_json::to_value(&before.transcript).unwrap()
        );
    }

    #[tokio::test]
    async fn missing_session_override_updates_and_clears_fail() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        assert!(store
            .update_agent_override(id, "build", AgentOverride::default())
            .await
            .unwrap_err()
            .contains("no stored session"));
        assert!(store
            .clear_agent_override(id, "build")
            .await
            .unwrap_err()
            .contains("no stored session"));
    }

    #[tokio::test]
    async fn concurrent_store_handles_preserve_sibling_agent_overrides() {
        let (dir, first) = temp_store();
        let second = HistoryStore::open(dir.path().join("sessions.db")).unwrap();
        let id = Ulid::new();
        first.record_session(id, "/w", "old", false).await.unwrap();
        let build = AgentOverride {
            model: Some("p/build".into()),
            variant: AgentVariantOverride::Off,
        };
        let plan = AgentOverride {
            model: Some("p/plan".into()),
            variant: AgentVariantOverride::Named("low".into()),
        };
        let (a, b) = tokio::join!(
            first.update_agent_override(id, "build", build.clone()),
            second.update_agent_override(id, "plan", plan.clone())
        );
        a.unwrap();
        b.unwrap();
        let saved = first.agent_overrides(id).await.unwrap().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved["build"], build);
        assert_eq!(saved["plan"], plan);
    }

    #[tokio::test]
    async fn malformed_agent_overrides_fail_reads_and_updates_explicitly() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store.record_session(id, "/w", "old", false).await.unwrap();
        for raw in [
            "{bad",
            "null",
            r#"{"build":{"model":null,"variant":{"kind":"unknown"}}}"#,
            r#"{"build":{"model":"","variant":{"kind":"inherit"}}}"#,
        ] {
            side_connection(&dir)
                .execute("UPDATE sessions SET agent_overrides=?1", [raw])
                .unwrap();
            assert!(store
                .meta(id)
                .await
                .unwrap_err()
                .contains("agent_overrides"));
            assert!(store.load_for_resume(id).await.is_err());
            assert!(store.list("/w").await.unwrap().is_empty());
            assert!(store.clear_agent_override(id, "build").await.is_err());
        }
    }

    #[tokio::test]
    async fn permission_writer_waits_for_outbox_write_ownership_without_poisoning_flush() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/ws", "model", false)
            .await
            .unwrap();
        let mut other = side_connection(&dir);
        let tx = other
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "UPDATE sessions SET title='outbox owner' WHERE id=?1",
            [id.to_string()],
        )
        .unwrap();
        store.append_event(
            id,
            EventEnvelope::new(
                id,
                0,
                EventKind::PermissionResolved,
                serde_json::json!({"request_id":Ulid::new(),"resolution":"allow_once"}),
            ),
        );
        let flushing = store.flush();
        tokio::pin!(flushing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut flushing)
                .await
                .is_err()
        );
        tx.commit().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut flushing)
            .await
            .unwrap()
            .unwrap();
        store.flush().await.unwrap();
        assert_eq!(store.replay(id, None, 0).await.unwrap().len(), 1);
    }

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
            .record_session(id, "/w", "cognition", true)
            .await
            .expect("record");
        store
            .record_session(id2, "/w", "default", false)
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.append_event(id, envelope(1, EventKind::MessageReceived));
        store.save_transcript(
            id,
            vec![ChatMessage::User {
                provenance: Default::default(),
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
        assert!(loaded.meta.editor);
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
            .record_session(id, "/w", "m", false)
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
            .record_session(id, "/w", "m", false)
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
        b.record_session(Ulid::new(), "/w", "m", false)
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
            .record_session(id, "/w", "m", false)
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
            .record_session(id, "/w", "m", true)
            .await
            .expect("record");
        store.append_event(id, envelope(0, EventKind::SessionStart));
        store.save_transcript(
            id,
            vec![ChatMessage::User {
                provenance: Default::default(),
                content: "hi".into(),
            }],
        );
        store.flush().await.expect("flush");

        let meta = store.meta(id).await.expect("meta").expect("present");
        assert!(meta.editor);
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
        store
            .record_session(parent, "/w", "m", false)
            .await
            .unwrap();
        store.record_session(child, "/w", "m", false).await.unwrap();
        store
            .create_child_link(
                child,
                "vigie".into(),
                "goal".into(),
                "{}".into(),
                4,
                EventEnvelope::new(parent, 0, EventKind::SubagentSpawned, serde_json::json!({})),
            )
            .await
            .unwrap();
        for status in ["running", "starting", "typo"] {
            let error = store
                .finish_child_link(child, status.into(), None, "wrong".into())
                .await
                .unwrap_err();
            assert!(error.contains("invalid terminal child status"));
            assert_eq!(
                store.child_link(child).await.unwrap().unwrap().status,
                "running"
            );
            assert!(store.pending_completions(parent).await.unwrap().is_empty());
            assert!(store.load_for_resume(child).await.unwrap().is_some());
        }
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
        store
            .record_session(parent, "/w", "m", false)
            .await
            .unwrap();
        store.record_session(child, "/w", "m", false).await.unwrap();
        store
            .create_child_link(
                child,
                "vigie".into(),
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
    async fn acked_append_reports_its_own_outcome_not_the_poison() {
        let (_dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", false)
            .await
            .expect("record");
        store.append_event(Ulid::new(), envelope(0, EventKind::SessionStart));
        store
            .flush()
            .await
            .expect_err("poisoned by an unknown session");
        let event = envelope(0, EventKind::SessionStart);
        store
            .append_event_acked(id, event.clone())
            .await
            .expect("its own append committed");
        store
            .flush()
            .await
            .expect_err("the poison is still reported");
        store
            .append_event_acked(id, event.clone())
            .await
            .expect("identical retry");
        let mut conflicting = event;
        conflicting.payload = serde_json::json!({"different": true});
        let error = store.append_event_acked(id, conflicting).await.unwrap_err();
        assert!(error.contains("conflicting event"), "{error}");
        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.envelopes.len(), 1);
        assert_eq!(loaded.meta.last_seq, 0);
        assert!(store.load_for_resume(id).await.unwrap().is_some());
        store
            .append_event_acked(Ulid::new(), envelope(1, EventKind::SessionStart))
            .await
            .expect_err("its own failure is reported");
    }

    #[tokio::test]
    async fn failed_append_poison_survives_multiple_flushes() {
        let (dir, store) = temp_store();
        let id = Ulid::new();
        store
            .record_session(id, "/w", "m", false)
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

        let result = store.record_session(Ulid::new(), "/w", "m", false).await;
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
            .record_session(known, "/w", "m", false)
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

    // Given a captain message with a trailing editor-context block
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
                "INSERT INTO sessions VALUES (?1, '/w', 'm', 'navigator', NULL, 't', 't', -1, NULL)",
                params![id.to_string()],
            )
            .expect("old row");
        }
        let store = HistoryStore::open(&path).expect("migrates");
        let meta = store.meta(id).await.expect("meta").expect("present");
        assert!(
            !meta.editor,
            "legacy profiles require explicit manual migration"
        );
        assert_eq!(meta.model_resolution, None);
        assert!(meta.agent_overrides.is_empty());
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
            .record_session_resolved(id, "/w", "m", false, None, Some(&pick("high")), None)
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
            .record_session(id, "/w", "m", false)
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
            .record_session_resolved(id, "/w", "m", false, Some(&make(300)), None, None)
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
                provenance: Default::default(),
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
        store.record_session(id, "/w", "m", false).await.unwrap();
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
        store.record_session(id, "/w", "m", false).await.unwrap();
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
