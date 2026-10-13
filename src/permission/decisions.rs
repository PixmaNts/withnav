//! The history of permission decisions stored in withnav.db.

use std::path::PathBuf;

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    rules::Effect,
    store::{path_text, Error, PermissionStore},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Allowed,
    Denied,
    Pending,
    Cancelled,
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::rules::{RuleOrigin, RuleScope};

    #[test]
    fn history_queries_use_ordered_indexes_without_table_scan_or_sort() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        for (filter, index) in [
            (DecisionFilter::default(), "permission_decisions_time"),
            (
                DecisionFilter {
                    session_id: Some("session".into()),
                    ..Default::default()
                },
                "permission_decisions_session_time",
            ),
        ] {
            let (sql, values) = history_query(&filter, 100).unwrap();
            let db = store.db();
            let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan = statement
                .query_map(rusqlite::params_from_iter(values), |row| {
                    row.get::<_, String>(3)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(plan.iter().any(|detail| detail.contains(index)), "{plan:?}");
            for detail in plan {
                assert!(!detail.contains("TEMP B-TREE"), "{detail}");
                // An ordered index walk is expected for unfiltered history,
                // but an unindexed full table scan is not.
                assert!(
                    !detail.contains("SCAN permission_decisions") || detail.contains("USING INDEX"),
                    "{detail}"
                );
            }
        }
    }

    #[test]
    fn session_migrations_preserve_permissions_in_both_open_orders() {
        use crate::session::store::HistoryStore;

        for history_first in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("withnav.db");
            let mut history = history_first.then(|| HistoryStore::open(&database).unwrap());
            let store = PermissionStore::open(dir.path()).unwrap();
            let rule = store
                .add(
                    "bash",
                    "git status *",
                    Effect::Allow,
                    RuleScope::Global,
                    RuleOrigin::Manual,
                )
                .unwrap();
            let decision = Decision {
                id: 0,
                time: crate::bus::event::now_rfc3339(),
                session_id: "session".into(),
                turn_id: "turn".into(),
                call_id: "call".into(),
                request_id: None,
                agent: "build".into(),
                workspace: dir.path().into(),
                tool: "bash".into(),
                call_snapshot: CallSnapshot {
                    arguments: "git status".into(),
                    truncated: false,
                },
                command_matches: serde_json::json!([{ "rule_id": rule.id }]),
                rule_id: Some(rule.id),
                effect: Some(Effect::Allow),
                flow_verdict: None,
                captain_answer: None,
                outcome: Outcome::Allowed,
                approved_by: Some("rule".into()),
            };
            let id = store.record(&decision).unwrap();
            if history.is_none() {
                history = Some(HistoryStore::open(&database).unwrap());
            }
            assert_eq!(
                store
                    .list(&crate::permission::rules::RuleFilter::default())
                    .unwrap(),
                vec![rule.clone()]
            );
            drop(store);
            drop(history);
            // Opening the session store again reruns its idempotent migration.
            let _history = HistoryStore::open(&database).unwrap();
            let store = PermissionStore::open(dir.path()).unwrap();
            assert_eq!(
                store
                    .matching("bash", "git status", dir.path(), "build")
                    .unwrap(),
                Some(rule)
            );
            let mut expected = decision;
            expected.id = id;
            expected.workspace = dir.path().canonicalize().unwrap();
            assert_eq!(
                store
                    .list_decisions(&DecisionFilter::default(), 100)
                    .unwrap(),
                vec![expected]
            );
        }
    }

    #[test]
    fn bounded_history_survives_revocation_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        let rule = store
            .add(
                "bash",
                "*",
                Effect::Allow,
                RuleScope::Global,
                RuleOrigin::Always,
            )
            .unwrap();
        let decision = Decision {
            id: 0,
            time: crate::bus::event::now_rfc3339(),
            session_id: "session".into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            request_id: Some("request".into()),
            agent: "build".into(),
            workspace: dir.path().into(),
            tool: "bash".into(),
            call_snapshot: CallSnapshot {
                arguments: format!("a{}", "é".repeat(3000)),
                truncated: false,
            },
            command_matches: serde_json::json!([{ "command": "git status", "rule_id": rule.id }]),
            rule_id: Some(rule.id),
            effect: Some(Effect::Allow),
            flow_verdict: Some("ask_captain".into()),
            captain_answer: None,
            outcome: Outcome::Pending,
            approved_by: None,
        };
        let id = store.record(&decision).unwrap();
        store.revoke(rule.id).unwrap();
        let answer = serde_json::json!({"answer": "once"});
        assert!(store
            .update_outcome(id, Outcome::Allowed, Some(&answer), Some("captain"))
            .unwrap());
        assert!(!store
            .update_outcome(id + 1, Outcome::Denied, None, None)
            .unwrap());
        drop(store);
        let store = PermissionStore::open(dir.path()).unwrap();
        let rows = store
            .list_decisions(&DecisionFilter::default(), 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.rule_id, Some(rule.id));
        assert_eq!(row.command_matches, decision.command_matches);
        assert_eq!(row.captain_answer.as_ref(), Some(&answer));
        assert_eq!(row.approved_by.as_deref(), Some("captain"));
        assert_eq!(row.outcome, Outcome::Allowed);
        assert_eq!(row.call_snapshot.arguments.len(), 4095);
        assert!(row.call_snapshot.truncated);
        assert!(store
            .list_decisions(&DecisionFilter::default(), 0)
            .unwrap()
            .is_empty());
        for filter in [
            DecisionFilter {
                session_id: Some("other".into()),
                ..Default::default()
            },
            DecisionFilter {
                agent: Some("plan".into()),
                ..Default::default()
            },
            DecisionFilter {
                outcome: Some(Outcome::Pending),
                ..Default::default()
            },
        ] {
            assert!(store.list_decisions(&filter, 10).unwrap().is_empty());
        }
        assert_eq!(
            store
                .list_decisions(
                    &DecisionFilter {
                        workspace: Some(dir.path().join(".")),
                        ..Default::default()
                    },
                    1
                )
                .unwrap()
                .len(),
            1
        );
    }
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Pending => "pending",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

/// The exact serialized arguments, bounded on record to 4 KiB of UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSnapshot {
    pub arguments: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Zero on input; the database assigns the durable identity.
    pub id: i64,
    pub time: String,
    pub session_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub request_id: Option<String>,
    pub agent: String,
    pub workspace: PathBuf,
    pub tool: String,
    pub call_snapshot: CallSnapshot,
    pub command_matches: Value,
    pub rule_id: Option<i64>,
    pub effect: Option<Effect>,
    pub flow_verdict: Option<String>,
    pub captain_answer: Option<Value>,
    pub outcome: Outcome,
    pub approved_by: Option<String>,
}

#[derive(Debug, Default)]
pub struct DecisionFilter {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub workspace: Option<PathBuf>,
    pub outcome: Option<Outcome>,
}

fn json_column<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn read_decision(row: &Row<'_>) -> rusqlite::Result<Decision> {
    let effect: Option<String> = row.get(12)?;
    let answer: Option<String> = row.get(14)?;
    let outcome: String = row.get(15)?;
    Ok(Decision {
        id: row.get(0)?,
        time: row.get(1)?,
        session_id: row.get(2)?,
        turn_id: row.get(3)?,
        call_id: row.get(4)?,
        request_id: row.get(5)?,
        agent: row.get(6)?,
        workspace: PathBuf::from(row.get::<_, String>(7)?),
        tool: row.get(8)?,
        call_snapshot: json_column(row, 9)?,
        command_matches: json_column(row, 10)?,
        rule_id: row.get(11)?,
        effect: effect.map(Effect::from_db).transpose()?,
        flow_verdict: row.get(13)?,
        captain_answer: answer
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        outcome: match outcome.as_str() {
            "allowed" => Outcome::Allowed,
            "denied" => Outcome::Denied,
            "pending" => Outcome::Pending,
            "cancelled" => Outcome::Cancelled,
            "expired" => Outcome::Expired,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        approved_by: row.get(16)?,
    })
}

fn history_query(
    filter: &DecisionFilter,
    limit: usize,
) -> Result<(String, Vec<rusqlite::types::Value>), Error> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(session) = &filter.session_id {
        clauses.push("session_id = ?");
        values.push(session.clone().into());
    }
    if let Some(agent) = &filter.agent {
        clauses.push("agent = ?");
        values.push(agent.clone().into());
    }
    if let Some(workspace) = &filter.workspace {
        clauses.push("workspace = ?");
        values.push(path_text(&workspace.canonicalize()?)?.to_owned().into());
    }
    if let Some(outcome) = filter.outcome {
        clauses.push("outcome = ?");
        values.push(outcome.as_str().to_owned().into());
    }
    let mut sql = String::from(
        "SELECT id,time,session_id,turn_id,call_id,request_id,agent,workspace,tool,
         call_snapshot,command_matches,rule_id,effect,flow_verdict,captain_answer,outcome,approved_by
         FROM permission_decisions",
    );
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY time DESC,id DESC LIMIT ?");
    values.push(
        i64::try_from(limit)
            .map_err(|_| Error::Invalid("history limit is too large".into()))?
            .into(),
    );
    Ok((sql, values))
}

/// Every variable text of a stored decision, after redaction.
const SNAPSHOT_BYTES: usize = 4096;
const MATCH_LABEL_BYTES: usize = 256;
const MATCHES_KEPT: usize = 32;
const ANSWER_BYTES: usize = 2048;
const SHORT_TEXT_BYTES: usize = 256;

/// Cut `text` at a character boundary; returns whether it was cut.
fn cut(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Secrets are redacted before anything is cut, so a cut never leaves part
/// of a token behind.
fn redact_text(text: &str) -> String {
    let mut value = Value::String(text.to_owned());
    if !crate::flows::redaction::contains_secret(&value) {
        return text.to_owned();
    }
    let words: Vec<String> = text
        .split_inclusive(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ':' | '='))
        .map(|word| {
            let (body, tail) = word.split_at(
                word.trim_end_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ':' | '=')
                })
                .len(),
            );
            value = Value::String(body.to_owned());
            if crate::flows::redaction::contains_secret(&value) {
                format!("<redacted>{tail}")
            } else {
                word.to_owned()
            }
        })
        .collect();
    words.concat()
}

/// The reason a permission database error gives clients and the decision
/// history. Database messages can quote paths, SQL and stored values, so
/// only a fixed phrase chosen from the SQLite error code leaves the process;
/// callers log the full error through tracing.
pub fn public_reason(error: &Error) -> &'static str {
    use rusqlite::ffi::ErrorCode;
    let Error::Database(rusqlite::Error::SqliteFailure(failure, _)) = error else {
        return "database error";
    };
    match failure.code {
        ErrorCode::DatabaseLocked => "database is locked",
        ErrorCode::DatabaseBusy => "database is busy",
        ErrorCode::ReadOnly => "database is read-only",
        ErrorCode::DiskFull => "disk is full",
        ErrorCode::ConstraintViolation => "rule write rejected",
        _ => "database error",
    }
}

fn bound_value(value: &mut Value, limit: usize) {
    match value {
        Value::String(text) => {
            *text = redact_text(text);
            cut(text, limit);
        }
        Value::Array(values) => values.iter_mut().for_each(|v| bound_value(v, limit)),
        Value::Object(values) => values.values_mut().for_each(|v| bound_value(v, limit)),
        _ => {}
    }
}

fn bound_matches(matches: &Value) -> Value {
    let mut matches = matches.clone();
    if let Value::Array(items) = &mut matches {
        let total = items.len();
        items.truncate(MATCHES_KEPT);
        if total > MATCHES_KEPT {
            items.push(serde_json::json!({"omitted": total - MATCHES_KEPT}));
        }
    }
    bound_value(&mut matches, MATCH_LABEL_BYTES);
    matches
}

fn bound_answer(answer: &Value) -> Value {
    let mut answer = answer.clone();
    bound_value(&mut answer, ANSWER_BYTES);
    answer
}

impl PermissionStore {
    pub fn record(&self, decision: &Decision) -> Result<i64, Error> {
        let mut snapshot = decision.call_snapshot.clone();
        snapshot.arguments = redact_text(&snapshot.arguments);
        snapshot.truncated |= cut(&mut snapshot.arguments, SNAPSHOT_BYTES);
        let command_matches = bound_matches(&decision.command_matches);
        let captain_answer = decision.captain_answer.as_ref().map(bound_answer);
        let short = |text: &Option<String>| {
            text.as_ref().map(|text| {
                let mut text = redact_text(text);
                cut(&mut text, SHORT_TEXT_BYTES);
                text
            })
        };
        let flow_verdict = short(&decision.flow_verdict);
        let approved_by = short(&decision.approved_by);
        let workspace = decision.workspace.canonicalize()?;
        let db = self.db();
        db.execute(
            "INSERT INTO permission_decisions
             (time,session_id,turn_id,call_id,request_id,agent,workspace,tool,call_snapshot,
              command_matches,rule_id,effect,flow_verdict,captain_answer,outcome,approved_by)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                decision.time,
                decision.session_id,
                decision.turn_id,
                decision.call_id,
                decision.request_id,
                decision.agent,
                path_text(&workspace)?,
                decision.tool,
                serde_json::to_string(&snapshot)?,
                serde_json::to_string(&command_matches)?,
                decision.rule_id,
                decision.effect.map(Effect::as_str),
                flow_verdict,
                captain_answer
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                decision.outcome.as_str(),
                approved_by
            ],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn update_outcome(
        &self,
        id: i64,
        outcome: Outcome,
        captain_answer: Option<&Value>,
        approved_by: Option<&str>,
    ) -> Result<bool, Error> {
        Ok(self.db().execute(
            "UPDATE permission_decisions SET outcome=?1,
             captain_answer=COALESCE(?2,captain_answer), approved_by=COALESCE(?3,approved_by)
             WHERE id=?4",
            params![
                outcome.as_str(),
                captain_answer
                    .map(bound_answer)
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                approved_by,
                id
            ],
        )? != 0)
    }

    /// Ends a decision that is still pending, and only then: one a later
    /// decision already settled keeps its outcome.
    pub fn cancel_if_pending(&self, id: i64) -> Result<bool, Error> {
        Ok(self.db().execute(
            "UPDATE permission_decisions SET outcome='cancelled' WHERE id=?1 AND outcome='pending'",
            params![id],
        )? != 0)
    }

    /// A session that restarts cannot answer the questions its previous
    /// process asked: they end as `expired`. Returns how many rows changed.
    pub fn expire_pending(&self, session_id: &str) -> Result<usize, Error> {
        Ok(self.db().execute(
            "UPDATE permission_decisions SET outcome='expired'
             WHERE session_id=?1 AND outcome='pending'",
            params![session_id],
        )?)
    }

    pub fn list_decisions(
        &self,
        filter: &DecisionFilter,
        limit: usize,
    ) -> Result<Vec<Decision>, Error> {
        let (sql, values) = history_query(filter, limit)?;
        let db = self.db();
        let mut statement = db.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), read_decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod public_reason_tests {
    use super::*;

    fn sqlite(code: rusqlite::ffi::ErrorCode, extended: i32, message: &str) -> Error {
        Error::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: extended,
            },
            Some(message.into()),
        ))
    }

    #[test]
    fn public_reasons_never_carry_the_database_message() {
        use rusqlite::ffi::{self, ErrorCode};
        let secret = "token=acme_live_Zq81kPw2\nat /home/user/.secret";
        for (error, reason) in [
            (
                sqlite(ErrorCode::DatabaseLocked, ffi::SQLITE_LOCKED, secret),
                "database is locked",
            ),
            (
                sqlite(ErrorCode::DatabaseBusy, ffi::SQLITE_BUSY, secret),
                "database is busy",
            ),
            (
                sqlite(ErrorCode::ReadOnly, ffi::SQLITE_READONLY, secret),
                "database is read-only",
            ),
            (
                sqlite(ErrorCode::DiskFull, ffi::SQLITE_FULL, secret),
                "disk is full",
            ),
            (
                sqlite(
                    ErrorCode::ConstraintViolation,
                    ffi::SQLITE_CONSTRAINT_TRIGGER,
                    secret,
                ),
                "rule write rejected",
            ),
            (
                sqlite(ErrorCode::SystemIoFailure, ffi::SQLITE_IOERR, secret),
                "database error",
            ),
            (Error::Invalid(secret.into()), "database error"),
            (Error::Io(std::io::Error::other(secret)), "database error"),
        ] {
            assert_eq!(public_reason(&error), reason);
        }
    }
}
