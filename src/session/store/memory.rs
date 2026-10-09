//! Memory writes share the history writer; recovery and search use snapshot reads.
use super::{read, HistoryStore, Write, WriteError};
use crate::agent::memory::{Episode, EpisodeBody};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn sql_seq(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn seq(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub session: String,
    pub workspace: String,
    pub anchor_seq: u64,
    pub text: String,
    pub start_seq: u64,
    pub end_seq: Option<u64>,
    pub progress: String,
    pub close_reason: Option<String>,
    pub writer_status: String,
    pub attempts: u32,
    pub claim: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub session: String,
    pub workspace: String,
    pub last_turn: u64,
    pub active_goal: Option<String>,
    pub version: u32,
}

pub use crate::agent::memory::component::Injection;

pub(super) enum Operation {
    Expire {
        goal: String,
    },
    Published {
        operation: Box<Operation>,
        event: crate::bus::event::EventEnvelope,
    },
    Observe {
        state: State,
        goals: Vec<Goal>,
    },
    Claim {
        goal: String,
        token: String,
    },
    Complete {
        goal: String,
        token: String,
        body: EpisodeBody,
        model: String,
        usage: String,
    },
    Fail {
        goal: String,
        token: String,
        error: String,
        skipped: bool,
    },
    Inject(Injection),
    Startup {
        session: String,
        part: Option<crate::agent::memory::component::Part>,
    },
}

pub(super) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_state (
            session TEXT PRIMARY KEY, workspace TEXT NOT NULL, last_turn INTEGER NOT NULL,
            active_goal TEXT, version INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS memory_goals (
            id TEXT PRIMARY KEY, session TEXT NOT NULL, workspace TEXT NOT NULL,
            anchor_seq INTEGER NOT NULL, goal_text TEXT NOT NULL, start_seq INTEGER NOT NULL,
            end_seq INTEGER, progress TEXT NOT NULL, close_reason TEXT,
            writer_status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
            claim TEXT, claimed_at INTEGER, last_error TEXT, created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS memory_goals_session ON memory_goals(session,start_seq);
        CREATE TABLE IF NOT EXISTS episodes (
            id INTEGER PRIMARY KEY, goal_id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
            workspace TEXT NOT NULL, start_seq INTEGER NOT NULL, end_seq INTEGER NOT NULL,
            close_reason TEXT NOT NULL, goal TEXT NOT NULL, body TEXT NOT NULL,
            search_text TEXT NOT NULL, writer_model TEXT NOT NULL, usage TEXT NOT NULL,
            created_at TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS episodes_workspace ON episodes(workspace,id DESC);
        CREATE VIRTUAL TABLE IF NOT EXISTS episodes_fts USING fts5(goal,search_text,content='episodes',content_rowid='id');
        CREATE TRIGGER IF NOT EXISTS episodes_insert AFTER INSERT ON episodes BEGIN
            INSERT INTO episodes_fts(rowid,goal,search_text) VALUES(new.id,new.goal,new.search_text); END;
        CREATE TRIGGER IF NOT EXISTS episodes_delete AFTER DELETE ON episodes BEGIN
            INSERT INTO episodes_fts(episodes_fts,rowid,goal,search_text) VALUES('delete',old.id,old.goal,old.search_text); END;
        CREATE TRIGGER IF NOT EXISTS episodes_update AFTER UPDATE ON episodes BEGIN
            INSERT INTO episodes_fts(episodes_fts,rowid,goal,search_text) VALUES('delete',old.id,old.goal,old.search_text);
            INSERT INTO episodes_fts(rowid,goal,search_text) VALUES(new.id,new.goal,new.search_text); END;
        CREATE TABLE IF NOT EXISTS memory_startup (session TEXT PRIMARY KEY, part TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS memory_injections (
            request_id TEXT PRIMARY KEY, session TEXT NOT NULL, turn TEXT NOT NULL,
            step TEXT NOT NULL, model TEXT NOT NULL, source TEXT NOT NULL,
            episode_ids TEXT NOT NULL, rendered_text TEXT NOT NULL, hash TEXT NOT NULL,
            created_at TEXT NOT NULL);"
    )
}

pub(super) fn delete(tx: &Transaction<'_>, session: &str) -> rusqlite::Result<()> {
    for table in [
        "memory_state",
        "memory_goals",
        "episodes",
        "memory_injections",
        "memory_startup",
    ] {
        tx.execute(&format!("DELETE FROM {table} WHERE session=?1"), [session])?;
    }
    Ok(())
}

pub(super) fn apply(tx: &Transaction<'_>, operation: Operation) -> Result<(), WriteError> {
    let now = crate::bus::event::now_rfc3339();
    match operation {
        Operation::Expire { goal } => {
            let changed = tx.execute("UPDATE memory_goals SET writer_status='failed',claim=NULL,last_error='writer lease expired after maximum attempts',updated_at=?2 WHERE id=?1 AND writer_status='writing' AND attempts>=2 AND claimed_at<unixepoch()-60", params![goal,now])?;
            if changed == 0 {
                return Err(WriteError::InvalidSelection("memory result stale".into()));
            }
        }
        Operation::Published { operation, event } => {
            apply(tx, *operation)?;
            tx.execute(
                "INSERT INTO events(session_id,seq,kind,envelope) VALUES(?1,?2,?3,?4)",
                params![
                    event.session_id.to_string(),
                    sql_seq(event.seq)?,
                    event.kind,
                    serde_json::to_string(&event)?
                ],
            )?;
            let touched = tx.execute(
                "UPDATE sessions SET updated_at=?2,last_seq=MAX(last_seq,?3) WHERE id=?1",
                params![event.session_id.to_string(), now, sql_seq(event.seq)?],
            )?;
            if touched == 0 {
                return Err(WriteError::UnknownSession(event.session_id));
            }
        }
        Operation::Observe { state, goals } => {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM sessions WHERE id=?1",
                    [&state.session],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(WriteError::InvalidSelection(
                    "memory session deleted".into(),
                ));
            }
            let previous: Option<u64> = tx
                .query_row(
                    "SELECT last_turn FROM memory_state WHERE session=?1",
                    [&state.session],
                    |r| seq(r, 0),
                )
                .optional()?;
            if previous.is_some_and(|turn| turn >= state.last_turn) {
                return Err(WriteError::InvalidSelection("memory result stale".into()));
            }
            for goal in goals {
                if goal.session != state.session
                    || goal.workspace != state.workspace
                    || goal.start_seq > goal.end_seq.unwrap_or(u64::MAX)
                {
                    return Err(WriteError::InvalidSelection(
                        "invalid memory goal scope or range".into(),
                    ));
                }
                tx.execute("INSERT INTO memory_goals(id,session,workspace,anchor_seq,goal_text,start_seq,end_seq,progress,close_reason,writer_status,created_at,updated_at)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                    ON CONFLICT(id) DO UPDATE SET end_seq=excluded.end_seq,progress=excluded.progress,close_reason=excluded.close_reason,writer_status=excluded.writer_status,updated_at=excluded.updated_at",
                    params![goal.id,goal.session,goal.workspace,sql_seq(goal.anchor_seq)?,goal.text,sql_seq(goal.start_seq)?,goal.end_seq.map(sql_seq).transpose()?,goal.progress,goal.close_reason,goal.writer_status,now])?;
            }
            tx.execute("INSERT INTO memory_state(session,workspace,last_turn,active_goal,version) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(session) DO UPDATE SET last_turn=excluded.last_turn,active_goal=excluded.active_goal,version=excluded.version",
                params![state.session,state.workspace,sql_seq(state.last_turn)?,state.active_goal,state.version])?;
        }
        Operation::Claim { goal, token } => {
            tx.execute("UPDATE memory_goals SET writer_status='writing',claim=?2,claimed_at=unixepoch(),attempts=attempts+1,updated_at=?3
                WHERE id=?1 AND end_seq IS NOT NULL AND attempts<2 AND (writer_status='pending' OR (writer_status='writing' AND claimed_at<unixepoch()-60))", params![goal,token,now])?;
        }
        Operation::Complete {
            goal,
            token,
            body,
            model,
            usage,
        } => {
            let raw = serde_json::to_string(&body)?;
            let row: Option<(String,u64,u64)> = tx.query_row("SELECT goal_text,start_seq,end_seq FROM memory_goals WHERE id=?1 AND claim=?2 AND writer_status='writing'", params![goal,token], |r| Ok((r.get(0)?,seq(r,1)?,seq(r,2)?))).optional()?;
            let Some((text, start, end)) = row else {
                return Err(WriteError::InvalidSelection("memory result stale".into()));
            };
            let mut query = tx.prepare("SELECT seq FROM events WHERE session_id=(SELECT session FROM memory_goals WHERE id=?1) AND seq BETWEEN ?2 AND ?3")?;
            let sequences = query
                .query_map(params![goal, sql_seq(start)?, sql_seq(end)?], |r| seq(r, 0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            crate::agent::memory::writer::parse(&raw, &text, start, end, &sequences)
                .map_err(WriteError::InvalidSelection)?;
            tx.execute("INSERT OR IGNORE INTO episodes(goal_id,session,workspace,start_seq,end_seq,close_reason,goal,body,search_text,writer_model,usage,created_at)
                SELECT id,session,workspace,start_seq,end_seq,close_reason,goal_text,?3,?3,?4,?5,?6 FROM memory_goals WHERE id=?1 AND claim=?2 AND writer_status='writing'",
                params![goal,token,raw,model,usage,now])?;
            tx.execute("UPDATE memory_goals SET writer_status='done',claim=NULL,last_error=NULL,updated_at=?3 WHERE id=?1 AND claim=?2", params![goal,token,now])?;
        }
        Operation::Fail {
            goal,
            token,
            error,
            skipped,
        } => {
            tx.execute("UPDATE memory_goals SET writer_status=CASE WHEN ?4 OR attempts>=2 THEN 'skipped' ELSE 'pending' END,claim=NULL,last_error=?3,updated_at=?5 WHERE id=?1 AND claim=?2", params![goal,token,error,skipped,now])?;
        }
        Operation::Inject(i) => {
            if !tx
                .query_row("SELECT 1 FROM sessions WHERE id=?1", [&i.session], |_| {
                    Ok(())
                })
                .optional()?
                .is_some()
            {
                return Err(WriteError::InvalidSelection(
                    "memory injection session deleted".into(),
                ));
            }
            tx.execute("INSERT INTO memory_injections(request_id,session,turn,step,model,source,episode_ids,rendered_text,hash,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![i.request_id,i.session,i.turn,i.step,i.model,i.source,serde_json::to_string(&i.episode_ids)?,i.text,i.hash,now])?;
        }
        Operation::Startup { session, part } => {
            tx.execute("INSERT INTO memory_startup(session,part) SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM sessions WHERE id=?1) ON CONFLICT(session) DO NOTHING", params![session, serde_json::to_string(&part)?])?;
        }
    }
    Ok(())
}

impl HistoryStore {
    pub(crate) async fn memory_publish_expired(
        &self,
        goal: String,
        event: crate::bus::event::EventEnvelope,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Published {
            operation: Box::new(Operation::Expire { goal }),
            event,
        }))
        .await
    }
    pub(crate) async fn memory_publish_observe(
        &self,
        state: State,
        goals: Vec<Goal>,
        event: crate::bus::event::EventEnvelope,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Published {
            operation: Box::new(Operation::Observe { state, goals }),
            event,
        }))
        .await
    }
    pub(crate) async fn memory_publish_complete(
        &self,
        goal: String,
        token: String,
        body: EpisodeBody,
        model: String,
        usage: String,
        event: crate::bus::event::EventEnvelope,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Published {
            operation: Box::new(Operation::Complete {
                goal,
                token,
                body,
                model,
                usage,
            }),
            event,
        }))
        .await
    }
    pub async fn memory_save_startup(
        &self,
        session: &str,
        part: Option<crate::agent::memory::component::Part>,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Startup {
            session: session.into(),
            part,
        }))
        .await
    }
    pub async fn memory_startup(
        &self,
        session: &str,
    ) -> Result<Option<crate::agent::memory::component::Part>, String> {
        let path = Arc::clone(&self.path);
        let session = session.to_owned();
        read::blocking(move || {
            read::with_read_tx(&path, |tx| {
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT part FROM memory_startup WHERE session=?1",
                        [session],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                raw.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
                    .transpose()
                    .map(Option::flatten)
            })
        })
        .await
    }
    pub async fn memory_observe(&self, state: State, goals: Vec<Goal>) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Observe { state, goals }))
            .await
    }
    pub async fn memory_claim(&self, goal: &str, token: &str) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Claim {
            goal: goal.into(),
            token: token.into(),
        }))
        .await
    }
    pub async fn memory_complete(
        &self,
        goal: &str,
        token: &str,
        body: EpisodeBody,
        model: String,
        usage: String,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Complete {
            goal: goal.into(),
            token: token.into(),
            body,
            model,
            usage,
        }))
        .await
    }
    pub async fn memory_fail(
        &self,
        goal: &str,
        token: &str,
        error: String,
        skipped: bool,
    ) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Fail {
            goal: goal.into(),
            token: token.into(),
            error,
            skipped,
        }))
        .await
    }
    pub async fn memory_inject(&self, injection: Injection) -> Result<(), String> {
        self.acked(Write::Memory(Operation::Inject(injection)))
            .await
    }
    pub async fn memory_state(&self, session: &str) -> Result<Option<State>, String> {
        let path = Arc::clone(&self.path);
        let session = session.to_owned();
        read::blocking(move || read::with_read_tx(&path, |tx| {
            tx.query_row("SELECT session,workspace,last_turn,active_goal,version FROM memory_state WHERE session=?1", [session], |r| Ok(State {session:r.get(0)?,workspace:r.get(1)?,last_turn:seq(r,2)?,active_goal:r.get(3)?,version:r.get(4)?})).optional().map_err(|e|e.to_string())
        })).await
    }
    pub async fn memory_goals(&self, session: &str) -> Result<Vec<Goal>, String> {
        let path = Arc::clone(&self.path);
        let session = session.to_owned();
        read::blocking(move || read::with_read_tx(&path, |tx| {
            let mut stmt = tx.prepare("SELECT id,session,workspace,anchor_seq,goal_text,start_seq,end_seq,progress,close_reason,writer_status,attempts,claim FROM memory_goals WHERE session=?1 ORDER BY start_seq").map_err(|e|e.to_string())?;
            let rows = stmt.query_map([session], |r| Ok(Goal { id:r.get(0)?,session:r.get(1)?,workspace:r.get(2)?,anchor_seq:seq(r,3)?,text:r.get(4)?,start_seq:seq(r,5)?,end_seq:r.get::<_,Option<i64>>(6)?.map(|v|u64::try_from(v).map_err(|e|rusqlite::Error::FromSqlConversionFailure(6,rusqlite::types::Type::Integer,Box::new(e)))).transpose()?,progress:r.get(7)?,close_reason:r.get(8)?,writer_status:r.get(9)?,attempts:r.get(10)?,claim:r.get(11)? })).map_err(|e|e.to_string())?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
        })).await
    }
    pub async fn memory_episodes(
        &self,
        workspace: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Episode>, String> {
        if query.is_some_and(|q| q.len() > 256) {
            return Err("recall query exceeds 256 bytes".into());
        }
        let path = Arc::clone(&self.path);
        let workspace = workspace.to_owned();
        let terms = query.map(|q| {
            q.split_whitespace()
                .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ")
        });
        read::blocking(move || read::with_read_tx(&path, |tx| {
            let search = terms.as_ref().is_some_and(|t| !t.is_empty());
            if terms.as_ref().is_some_and(|t| t.is_empty()) { return Ok(vec![]); }
            let sql = if search { "SELECT e.id,e.session,e.created_at,e.start_seq,e.end_seq,e.body FROM episodes e JOIN episodes_fts f ON f.rowid=e.id WHERE e.workspace=?1 AND episodes_fts MATCH ?2 ORDER BY e.id DESC LIMIT ?3" } else { "SELECT id,session,created_at,start_seq,end_seq,body FROM episodes WHERE workspace=?1 AND ?2 IS NULL ORDER BY id DESC LIMIT ?3" };
            let mut stmt = tx.prepare(sql).map_err(|e|e.to_string())?;
            let rows = stmt.query_map(params![workspace,terms,limit.min(5) as i64], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,seq(r,3)?,seq(r,4)?,r.get::<_,String>(5)?))).map_err(|e|e.to_string())?;
            rows.map(|r| { let (id,session,created_at,start_seq,end_seq,raw)=r.map_err(|e|e.to_string())?; Ok(Episode {id,session,created_at,start_seq,end_seq,body:serde_json::from_str(&raw).map_err(|e|e.to_string())?}) }).collect()
        })).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_search_scope_and_delete_are_atomic() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO episodes VALUES(1,'g','s','/a',1,2,'model_done','punctuation task','{}','checked','fake','{}','now')", []).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH '\"task\"'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM episodes WHERE workspace='/b'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        {
            let tx = conn.transaction().unwrap();
            delete(&tx, "s").unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        {
            let tx = conn.transaction().unwrap();
            delete(&tx, "s").unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH '\"task\"'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
