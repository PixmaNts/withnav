//! Permission rules stored in withnav.db and the query that finds the rule applying to a call.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::store::{path_text, Error, PermissionStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(
        store: &PermissionStore,
        action: &str,
        resource: &str,
        effect: Effect,
        scope: RuleScope,
    ) -> Rule {
        store
            .add(action, resource, effect, scope, RuleOrigin::Manual)
            .unwrap()
    }

    #[test]
    fn precedence_is_precision_scope_effect_then_recency() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        let matched = || {
            store
                .matching("bash", "git status", dir.path(), "build")
                .unwrap()
                .unwrap()
        };
        add(
            &store,
            "*",
            "*",
            Effect::Deny,
            RuleScope::Agent("build".into()),
        );
        let global = add(
            &store,
            "bash",
            "git status",
            Effect::Allow,
            RuleScope::Global,
        );
        assert_eq!(matched().id, global.id);
        let project = add(
            &store,
            "bash",
            "git status",
            Effect::Allow,
            RuleScope::Project(dir.path().into()),
        );
        assert_eq!(matched().id, project.id);
        let agent = add(
            &store,
            "bash",
            "git status",
            Effect::Allow,
            RuleScope::Agent("build".into()),
        );
        assert_eq!(matched().id, agent.id);
        let ask = add(
            &store,
            "bash",
            "git status",
            Effect::Ask,
            RuleScope::Agent("build".into()),
        );
        assert_eq!(matched().id, ask.id);
        let deny = add(
            &store,
            "bash",
            "git status",
            Effect::Deny,
            RuleScope::Agent("build".into()),
        );
        add(
            &store,
            "bash",
            "git status",
            Effect::Ask,
            RuleScope::Agent("build".into()),
        );
        assert_eq!(matched().id, deny.id);
        let newest = add(
            &store,
            "bash",
            "git status",
            Effect::Deny,
            RuleScope::Agent("build".into()),
        );
        assert_eq!(matched().id, newest.id);
        assert!(store.revoke(newest.id).unwrap());
        assert!(!store.revoke(newest.id).unwrap());
        assert_eq!(matched().id, deny.id);
        assert_eq!(
            store
                .list(&RuleFilter {
                    include_revoked: true,
                    scope: None
                })
                .unwrap()
                .len(),
            store.list(&RuleFilter::default()).unwrap().len() + 1
        );
    }

    #[test]
    fn scopes_are_exact_and_projects_are_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        let rule = add(
            &store,
            "write",
            "*",
            Effect::Allow,
            RuleScope::Project(dir.path().join(".")),
        );
        assert_eq!(
            store
                .matching("write", "a", dir.path(), "plan")
                .unwrap()
                .unwrap()
                .id,
            rule.id
        );
        assert!(store
            .matching("write", "a", other.path(), "plan")
            .unwrap()
            .is_none());
        let agent = add(
            &store,
            "edit",
            "*",
            Effect::Deny,
            RuleScope::Agent("plan".into()),
        );
        assert_eq!(
            store
                .matching("edit", "a", other.path(), "plan")
                .unwrap()
                .unwrap()
                .id,
            agent.id
        );
        assert!(store
            .matching("edit", "a", dir.path(), "build")
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .list(&RuleFilter {
                    scope: Some(RuleScope::Project(dir.path().join("."))),
                    ..Default::default()
                })
                .unwrap(),
            vec![rule]
        );
    }

    #[test]
    fn wildcard_and_literal_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        for value in ["a?b", "a[b", "a*b", "a\\*b", "a\\b", "name *"] {
            let rule = add(
                &store,
                &literal("read*"),
                &literal(value),
                Effect::Allow,
                RuleScope::Global,
            );
            assert_eq!(
                store
                    .matching("read*", value, dir.path(), "build")
                    .unwrap()
                    .unwrap()
                    .id,
                rule.id
            );
            assert!(store
                .matching(
                    "read*",
                    &value.replace(['?', '[', '*', '\\'], "x"),
                    dir.path(),
                    "build"
                )
                .unwrap()
                .is_none());
            assert!(store
                .matching("readXYZ", value, dir.path(), "build")
                .unwrap()
                .is_none());
            store.revoke(rule.id).unwrap();
        }
        let rule = add(&store, "read", "a?[b", Effect::Allow, RuleScope::Global);
        assert!(store
            .matching("read", "axb", dir.path(), "build")
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .matching("read", "a?[b", dir.path(), "build")
                .unwrap()
                .unwrap()
                .id,
            rule.id
        );
        add(
            &store,
            "bash",
            "git status *",
            Effect::Allow,
            RuleScope::Global,
        );
        for command in ["git status", "git status --short", "git status a/b"] {
            assert!(store
                .matching("bash", command, dir.path(), "build")
                .unwrap()
                .is_some());
        }
        assert!(store
            .matching("bash", "git statusx", dir.path(), "build")
            .unwrap()
            .is_none());
        add(&store, "write", "*", Effect::Ask, RuleScope::Global);
        assert!(store
            .matching("write", "a/b/c", dir.path(), "build")
            .unwrap()
            .is_some());
    }

    #[test]
    fn fresh_database_is_empty_and_database_errors_surface() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        assert!(store.list(&RuleFilter::default()).unwrap().is_empty());
        assert!(store
            .matching("bash", "ls", dir.path(), "build")
            .unwrap()
            .is_none());
        store
            .db()
            .execute_batch("DROP TABLE permission_rules")
            .unwrap();
        assert!(matches!(
            store.matching("bash", "ls", dir.path(), "build"),
            Err(Error::Database(_))
        ));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    Global,
    Project(PathBuf),
    Agent(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    Always,
    Manual,
    Flow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub action: String,
    pub resource: String,
    pub effect: Effect,
    pub scope: RuleScope,
    pub origin: RuleOrigin,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Default)]
pub struct RuleFilter {
    pub include_revoked: bool,
    pub scope: Option<RuleScope>,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    pub(crate) fn from_db(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

impl RuleScope {
    pub(crate) fn storage(&self) -> Result<(&'static str, String), Error> {
        Ok(match self {
            Self::Global => ("global", String::new()),
            Self::Project(path) => (
                "project",
                path_text(&crate::tools::canonical_path(path)?)?.into(),
            ),
            Self::Agent(name) if !name.is_empty() => ("agent", name.clone()),
            Self::Agent(_) => return Err(Error::Invalid("agent name must not be empty".into())),
        })
    }
}

/// Escape a concrete value for the permission pattern language. Only `*` is
/// a wildcard; backslash quotes a star or another backslash.
pub fn literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('*', "\\*")
}

fn glob(pattern: &str) -> (String, usize) {
    let mut output = String::new();
    let mut fixed = 0;
    let mut chars = pattern.chars().peekable();
    while let Some(mut ch) = chars.next() {
        let quoted = ch == '\\' && matches!(chars.peek(), Some('*' | '\\'));
        if quoted {
            ch = chars.next().expect("quoted character");
        }
        match ch {
            '*' if !quoted => output.push('*'),
            '*' => output.push_str("[*]"),
            '?' => output.push_str("[?]"),
            '[' => output.push_str("[[]"),
            _ => output.push(ch),
        }
        if ch != '*' || quoted {
            fixed += 1;
        }
    }
    (output, fixed)
}

fn bare_glob(pattern: &str) -> Option<String> {
    pattern.strip_suffix(" *").map(|bare| glob(bare).0)
}

#[cfg(windows)]
fn path_rule_key(value: &str) -> &str {
    value.strip_prefix(r"\\?\").unwrap_or(value)
}

#[cfg(windows)]
fn file_action(action: &str) -> bool {
    matches!(
        action,
        "read" | "write" | "edit" | "grep" | "glob" | "external_directory"
    )
}

const COLUMNS: &str = "id,action,resource,effect,scope,scope_value,origin,created_at,revoked_at";

fn read_rule(row: &Row<'_>) -> rusqlite::Result<Rule> {
    let scope: String = row.get(4)?;
    let value: String = row.get(5)?;
    let origin: String = row.get(6)?;
    Ok(Rule {
        id: row.get(0)?,
        action: row.get(1)?,
        resource: row.get(2)?,
        effect: Effect::from_db(row.get(3)?)?,
        scope: match scope.as_str() {
            "global" => RuleScope::Global,
            "project" => RuleScope::Project(value.into()),
            "agent" => RuleScope::Agent(value),
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        origin: match origin.as_str() {
            "always" => RuleOrigin::Always,
            "manual" => RuleOrigin::Manual,
            "flow" => RuleOrigin::Flow,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        created_at: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

fn add_rule(
    db: &rusqlite::Connection,
    action: &str,
    resource: &str,
    effect: Effect,
    scope: RuleScope,
    origin: RuleOrigin,
) -> Result<Rule, Error> {
    if action.is_empty() || action.contains('\0') || resource.contains('\0') {
        return Err(Error::Invalid(
            "rule action must not be empty; patterns cannot contain NUL".into(),
        ));
    }
    let (scope_kind, scope_value) = scope.storage()?;
    let (action_glob, action_precision) = glob(action);
    #[cfg(windows)]
    let resource_match = if file_action(action) {
        resource
            .strip_prefix(&literal(r"\\?\"))
            .unwrap_or_else(|| path_rule_key(resource))
    } else {
        resource
    };
    #[cfg(unix)]
    let resource_match = resource;
    let (resource_glob, resource_precision) = glob(resource_match);
    let origin = match origin {
        RuleOrigin::Always => "always",
        RuleOrigin::Manual => "manual",
        RuleOrigin::Flow => "flow",
    };
    db.execute(
        "INSERT INTO permission_rules
            (action,resource,action_glob,resource_glob,action_bare_glob,resource_bare_glob,
             precision,effect,scope,scope_value,origin,created_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            action,
            resource,
            action_glob,
            resource_glob,
            bare_glob(action),
            bare_glob(resource_match),
            (action_precision + resource_precision) as i64,
            effect.as_str(),
            scope_kind,
            scope_value,
            origin,
            crate::bus::event::now_rfc3339()
        ],
    )?;
    Ok(db.query_row(
        &format!("SELECT {COLUMNS} FROM permission_rules WHERE id=?1"),
        [db.last_insert_rowid()],
        read_rule,
    )?)
}

impl PermissionStore {
    pub fn add(
        &self,
        action: &str,
        resource: &str,
        effect: Effect,
        scope: RuleScope,
        origin: RuleOrigin,
    ) -> Result<Rule, Error> {
        add_rule(&self.db(), action, resource, effect, scope, origin)
    }

    /// Adds every rule or none: an "Always" answer covering several commands
    /// must not leave half of them saved.
    pub fn add_all(
        &self,
        rules: &[(String, String, Effect, RuleScope)],
        origin: RuleOrigin,
    ) -> Result<Vec<Rule>, Error> {
        let db = self.db();
        db.execute_batch("BEGIN IMMEDIATE")?;
        let saved: Result<Vec<Rule>, Error> = rules
            .iter()
            .map(|(action, resource, effect, scope)| {
                add_rule(&db, action, resource, *effect, scope.clone(), origin)
            })
            .collect();
        match saved {
            Ok(saved) => {
                db.execute_batch("COMMIT")?;
                Ok(saved)
            }
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Returns false for an unknown or already revoked rule.
    pub fn revoke(&self, id: i64) -> Result<bool, Error> {
        Ok(self.db().execute(
            "UPDATE permission_rules SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
            params![crate::bus::event::now_rfc3339(), id],
        )? != 0)
    }

    pub fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, Error> {
        let scope = filter.scope.as_ref().map(RuleScope::storage).transpose()?;
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {COLUMNS} FROM permission_rules
             WHERE (?1 OR revoked_at IS NULL)
             AND (?2 IS NULL OR (scope=?2 AND scope_value=?3)) ORDER BY id DESC"
        ))?;
        let rows = statement.query_map(
            params![
                filter.include_revoked,
                scope.as_ref().map(|s| s.0),
                scope.as_ref().map(|s| &s.1)
            ],
            read_rule,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn matching(
        &self,
        action: &str,
        resource: &str,
        workspace: &Path,
        agent: &str,
    ) -> Result<Option<Rule>, Error> {
        if action.contains('\0') || resource.contains('\0') {
            return Err(Error::Invalid(
                "permission values cannot contain NUL".into(),
            ));
        }
        let workspace = crate::tools::canonical_path(workspace)?;
        #[cfg(windows)]
        let resource = if file_action(action) {
            path_rule_key(resource)
        } else {
            resource
        };
        Ok(self
            .db()
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM permission_rules WHERE revoked_at IS NULL
             AND (?1 GLOB action_glob OR ?1 GLOB action_bare_glob)
             AND (?2 GLOB resource_glob OR ?2 GLOB resource_bare_glob)
             AND (scope='global' OR (scope='project' AND scope_value=?3)
                  OR (scope='agent' AND scope_value=?4))
             ORDER BY precision DESC,
                CASE scope WHEN 'agent' THEN 2 WHEN 'project' THEN 1 ELSE 0 END DESC,
                CASE effect WHEN 'deny' THEN 2 WHEN 'ask' THEN 1 ELSE 0 END DESC,
                created_at DESC, id DESC LIMIT 1"
                ),
                params![action, resource, path_text(&workspace)?, agent],
                read_rule,
            )
            .optional()?)
    }
}
