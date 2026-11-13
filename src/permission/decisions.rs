//! The history of permission decisions stored in withnav.db.

use std::path::PathBuf;

use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    rules::Effect,
    store::{Error, PermissionStore, path_text},
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
                flow_trace: None,
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
            expected.workspace = crate::tools::canonical_path(dir.path()).unwrap();
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
            flow_trace: Some(
                serde_json::json!({"protected_check": {"verdict": "failed", "score": 0.25}}),
            ),
            captain_answer: None,
            outcome: Outcome::Pending,
            approved_by: None,
        };
        let id = store.record(&decision).unwrap();
        store.revoke(rule.id).unwrap();
        let answer = serde_json::json!({"answer": "once"});
        assert!(
            store
                .update_outcome(id, Outcome::Allowed, Some(&answer), Some("captain"))
                .unwrap()
        );
        assert!(
            !store
                .update_outcome(id + 1, Outcome::Denied, None, None)
                .unwrap()
        );
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
        assert_eq!(row.flow_trace, decision.flow_trace);
        assert_eq!(row.outcome, Outcome::Allowed);
        assert_eq!(row.call_snapshot.arguments.len(), 4095);
        assert!(row.call_snapshot.truncated);
        assert!(
            store
                .list_decisions(&DecisionFilter::default(), 0)
                .unwrap()
                .is_empty()
        );
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

    fn decision(session_id: &str, outcome: Outcome, workspace: &std::path::Path) -> Decision {
        Decision {
            id: 0,
            time: crate::bus::event::now_rfc3339(),
            session_id: session_id.into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            request_id: None,
            agent: "build".into(),
            workspace: workspace.to_path_buf(),
            tool: "bash".into(),
            call_snapshot: CallSnapshot {
                arguments: "git status".into(),
                truncated: false,
            },
            command_matches: serde_json::json!([]),
            rule_id: None,
            effect: None,
            flow_verdict: None,
            flow_trace: None,
            captain_answer: None,
            outcome,
            approved_by: None,
        }
    }

    fn sessions_and_ids(store: &PermissionStore, limit: usize) -> Vec<(String, i64)> {
        store
            .list_decisions(&DecisionFilter::default(), limit)
            .unwrap()
            .into_iter()
            .map(|row| (row.session_id, row.id))
            .collect()
    }

    #[test]
    fn forgetting_one_conversation_leaves_the_other_conversations_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        for session in ["a", "b", "a", "c"] {
            store
                .record(&decision(session, Outcome::Allowed, dir.path()))
                .unwrap();
        }

        assert_eq!(store.forget_session("a").unwrap(), 2);
        let mut left = sessions_and_ids(&store, 10)
            .into_iter()
            .map(|(session, _)| session)
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(left, ["b", "c"]);
        assert!(
            store
                .list_decisions(
                    &DecisionFilter {
                        session_id: Some("a".into()),
                        ..Default::default()
                    },
                    10
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.forget_session("a").unwrap(),
            0,
            "forgetting twice is not an error"
        );
        assert_eq!(sessions_and_ids(&store, 10).len(), 2);
    }

    #[test]
    fn pruning_keeps_the_newest_settled_rows_and_every_pending_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        let oldest_pending = store
            .record(&decision("open", Outcome::Pending, dir.path()))
            .unwrap();
        let settled = (0..5)
            .map(|_| {
                store
                    .record(&decision("settled", Outcome::Allowed, dir.path()))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let newest_pending = store
            .record(&decision("open", Outcome::Pending, dir.path()))
            .unwrap();

        assert_eq!(store.prune_decisions(2).unwrap(), 3);
        let kept = sessions_and_ids(&store, 10)
            .into_iter()
            .map(|(_, id)| id)
            .collect::<Vec<_>>();
        assert_eq!(kept.len(), 4, "two settled rows and both pending ones");
        assert_eq!(kept.iter().filter(|id| **id == newest_pending).count(), 1);
        assert_eq!(
            kept.iter().filter(|id| **id == oldest_pending).count(),
            1,
            "an old open question is never pruned: {kept:?}"
        );
        for id in &settled[3..] {
            assert_eq!(kept.iter().filter(|kept| *kept == id).count(), 1);
        }
        for id in &settled[..3] {
            assert!(!kept.contains(id), "row {id} is past the ceiling");
        }
        // The rows that were kept are still answerable: the whole point of
        // excluding pending rows is that the Captain's answer still lands.
        assert!(
            store
                .update_outcome(oldest_pending, Outcome::Allowed, None, Some("captain"))
                .unwrap()
        );
        assert!(
            store
                .update_outcome(newest_pending, Outcome::Denied, None, None)
                .unwrap()
        );
    }

    #[test]
    fn recording_past_the_ceiling_prunes_the_oldest_settled_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::in_memory().unwrap();
        for _ in 0..MAX_DECISION_ROWS + 2 {
            store
                .record(&decision("session", Outcome::Allowed, dir.path()))
                .unwrap();
        }

        let rows = sessions_and_ids(&store, MAX_DECISION_ROWS * 2);
        assert_eq!(
            rows.len(),
            MAX_DECISION_ROWS,
            "the table stops at the ceiling: {}",
            rows.len()
        );
        let oldest = rows.iter().map(|(_, id)| *id).min().unwrap();
        assert_eq!(
            oldest, 3,
            "the two rows past the ceiling were the oldest ones"
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
    /// The flow runs behind the decision: their run ids in the flow run log
    /// and the score of each node that asked Instinct (see `nav permissions history`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_trace: Option<Value>,
    pub captain_answer: Option<Value>,
    pub outcome: Outcome,
    pub approved_by: Option<String>,
}

/// The scores behind a decision on one line, e.g.
/// `protected_check failed 0.25; permission-default requested 0.04 read_only 0.01`.
pub fn scores_summary(decision: &Decision) -> String {
    decision
        .flow_trace
        .as_ref()
        .map(trace_summary)
        .unwrap_or_default()
}

/// [`scores_summary`] of a `flow_trace` value.
pub fn trace_summary(trace: &Value) -> String {
    let mut parts = Vec::new();
    let check = &trace["protected_check"];
    if let Some(verdict) = check["verdict"].as_str() {
        parts.push(match check["score"].as_f64() {
            Some(score) => format!("protected_check {verdict} {score}"),
            None => format!("protected_check {verdict}"),
        });
    }
    for run in trace["flows"].as_array().into_iter().flatten() {
        let scores = run["scores"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(node, score)| Some(format!("{node} {}", score.as_f64()?)))
            .collect::<Vec<_>>();
        if !scores.is_empty() {
            parts.push(format!(
                "{} {}",
                run["flow"].as_str().unwrap_or_default(),
                scores.join(" ")
            ));
        }
    }
    parts.join("; ")
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
        flow_trace: row
            .get::<_, Option<String>>(17)?
            .and_then(|text| serde_json::from_str(&text).ok()),
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
        values.push(
            path_text(&crate::tools::canonical_path(workspace)?)?
                .to_owned()
                .into(),
        );
    }
    if let Some(outcome) = filter.outcome {
        clauses.push("outcome = ?");
        values.push(outcome.as_str().to_owned().into());
    }
    let mut sql = String::from(
        "SELECT id,time,session_id,turn_id,call_id,request_id,agent,workspace,tool,
         call_snapshot,command_matches,rule_id,effect,flow_verdict,captain_answer,outcome,approved_by,
         flow_trace
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

/// How many settled decisions are kept. The table exists to show the Captain
/// which questions could become rules, not to be a ledger of every call (owner
/// decision of 2026-11-10), so it is bounded rather than kept for the life of
/// the installation: a call argument the user never turns into a rule is not
/// worth storing forever. `nav permissions history` prints the newest 100 by
/// default and the flow captures are bounded at 1000 records a workspace, so a
/// thousand recent decisions is well past what rule promotion reads: a row is
/// a 4 KiB snapshot plus the bounded matches, trace and answer, which keeps
/// the whole table to single-digit MiB.
pub const MAX_DECISION_ROWS: usize = 1_000;

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
    // Redact the complete evidence before pruning commands: truncation must
    // not discard the only evidence that a secret was removed.
    bound_value(&mut matches, MATCH_LABEL_BYTES);
    if let Value::Array(items) = &mut matches {
        let total = items.len();
        items.truncate(MATCHES_KEPT);
        if total > MATCHES_KEPT {
            items.push(serde_json::json!({"omitted": total - MATCHES_KEPT}));
        }
    }
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
        let flow_trace = decision.flow_trace.clone().map(|mut trace| {
            bound_value(&mut trace, SHORT_TEXT_BYTES);
            trace
        });
        let approved_by = short(&decision.approved_by);
        let workspace = crate::tools::canonical_path(&decision.workspace)?;
        let db = self.db();
        db.execute(
            "INSERT INTO permission_decisions
             (time,session_id,turn_id,call_id,request_id,agent,workspace,tool,call_snapshot,
              command_matches,rule_id,effect,flow_verdict,captain_answer,outcome,approved_by,
              flow_trace)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
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
                approved_by,
                flow_trace.as_ref().map(serde_json::to_string).transpose()?
            ],
        )?;
        let id = db.last_insert_rowid();
        // The prune runs only once a row's id passes the ceiling. Ids are
        // AUTOINCREMENT, so they never repeat and never shrink when rows are
        // deleted: while the newest id is at or below the ceiling no more than
        // that many rows were ever inserted, so the ceiling cannot be exceeded
        // yet, and the write path stays free of a whole-history walk per
        // recorded call. Past it, the ceiling is restored immediately.
        if id > MAX_DECISION_ROWS as i64 {
            prune_settled(&db, MAX_DECISION_ROWS)?;
        }
        Ok(id)
    }

    /// Deletes every decision row of one conversation. `session_id` is the
    /// conversation's id in the form the table stores it, the same key
    /// `expire_pending` uses. Returns how many rows were removed.
    pub fn forget_session(&self, session_id: &str) -> Result<usize, Error> {
        Ok(self.db().execute(
            "DELETE FROM permission_decisions WHERE session_id = ?1",
            params![session_id],
        )?)
    }

    /// Prunes the decision history down to the newest `max` settled rows, the
    /// pass a retention bound needs. Returns how many rows were removed.
    /// Nothing is removed while fewer settled rows exist, and a ceiling of 0
    /// prunes nothing.
    pub fn prune_decisions(&self, max: usize) -> Result<usize, Error> {
        prune_settled(&self.db(), max)
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

/// Deletes the settled decisions older than the newest `max`, and nothing
/// else.
///
/// A row still `pending` is never pruned, whatever the ceiling: an open
/// question is settled by row id once the Captain answers (`update_outcome`,
/// `cancel_if_pending`), so pruning its row would lose the answer silently.
/// Keeping a handful of pending rows above the ceiling costs nothing, and the
/// ceiling is an upper bound on stored call arguments, not a target.
///
/// The subquery is the oldest of the kept rows, so the delete itself is a
/// range on the primary key: it touches only the rows it removes. While fewer
/// than `max` settled rows exist the subquery is that same oldest row and
/// nothing is removed; with `max` at 0 the subquery is empty, so nothing is
/// removed either.
fn prune_settled(db: &rusqlite::Connection, max: usize) -> Result<usize, Error> {
    let max = i64::try_from(max)
        .map_err(|_| Error::Invalid("decision retention ceiling is too large".into()))?;
    Ok(db.execute(
        "DELETE FROM permission_decisions
          WHERE outcome <> 'pending'
            AND id < (SELECT MIN(id) FROM (
                        SELECT id FROM permission_decisions
                         WHERE outcome <> 'pending'
                         ORDER BY id DESC LIMIT ?1))",
        params![max],
    )?)
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
