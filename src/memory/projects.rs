//! Projects, groups, and the links between sessions, projects and groups.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Row, params};
use tokio::io::AsyncReadExt;

use super::store::{Actor, Error, MemoryStore, new_id, now, one_line, required};

/// The longest project or group name, in characters.
pub const MAX_NAME_CHARS: usize = 100;

/// What a project alias is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AliasKind {
    GitCommonDir,
    Path,
}

text_enum!(AliasKind, "alias kind", {
    GitCommonDir => "git_common_dir",
    Path => "path",
});

/// What identifies the project a workspace belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub alias: String,
    pub kind: AliasKind,
    /// The name a project first seen through this alias takes.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAlias {
    pub alias: String,
    pub kind: AliasKind,
    pub added_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub created_at: String,
    /// Whether the Captain answered which group the project belongs to. `false`: still to ask.
    pub group_answered: bool,
    pub aliases: Vec<ProjectAlias>,
    /// The ids of the groups it shares with, retired ones left out.
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub project: Project,
    /// Whether this call created the project: the moment to ask its group.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub retired_at: Option<String>,
    /// The ids of its projects.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLink {
    pub session_id: String,
    pub project_id: String,
    pub active: bool,
}

/// The two links whose changes are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    ProjectGroup {
        project_id: String,
        group_id: String,
    },
    SessionProject {
        session_id: String,
        project_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Add,
    Remove,
}

text_enum!(ChangeKind, "membership change", {
    Add => "add",
    Remove => "remove",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeStatus {
    Applied,
    Proposed,
    Rejected,
}

text_enum!(ChangeStatus, "membership status", {
    Applied => "applied",
    Proposed => "proposed",
    Rejected => "rejected",
});

/// Who asked for a membership change, and from which session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBy {
    pub actor: Actor,
    pub session_id: Option<String>,
}

impl ChangedBy {
    #[must_use]
    pub const fn new(actor: Actor, session_id: Option<String>) -> Self {
        Self { actor, session_id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipChange {
    pub id: i64,
    pub relation: Relation,
    pub change: ChangeKind,
    pub status: ChangeStatus,
    pub actor: Actor,
    pub session_id: Option<String>,
    pub at: String,
    pub decided_at: Option<String>,
}

impl Relation {
    /// The relation's name, subject and object as the history table keeps them.
    fn columns(&self) -> (&'static str, &str, &str) {
        match self {
            Self::ProjectGroup {
                project_id,
                group_id,
            } => ("project_group", project_id, group_id),
            Self::SessionProject {
                session_id,
                project_id,
            } => ("session_project", session_id, project_id),
        }
    }

    fn from_columns(relation: &str, subject: String, object: String) -> Result<Self, Error> {
        match relation {
            "project_group" => Ok(Self::ProjectGroup {
                project_id: subject,
                group_id: object,
            }),
            "session_project" => Ok(Self::SessionProject {
                session_id: subject,
                project_id: object,
            }),
            _ => Err(Error::invalid(format!("unknown relation: {relation}"))),
        }
    }
}

/// How long git may take to name a repository. Past it the workspace gets no project for now:
/// answering with the folder instead would record a second, wrong project for a repository
/// that was only slow once.
const GIT_DEADLINE: Duration = Duration::from_secs(2);
/// The most git output read: one path.
const MAX_GIT_OUTPUT: u64 = 16 * 1024;

/// What identifies the project `workspace` belongs to, read from the machine on each call.
///
/// Inside a git repository it is the canonical common directory (`git rev-parse
/// --git-common-dir`): every linked worktree of a repository shares it, so they share one
/// project, while a separate clone has its own and stays its own project. Outside git, or
/// without git on the machine, it is the canonical folder.
///
/// # Errors
///
/// When the folder does not exist, a path is not UTF-8, or git does not answer within two
/// seconds. The caller then goes on without a project.
pub async fn identity(workspace: &Path) -> Result<ProjectIdentity, Error> {
    let workspace = tokio::fs::canonicalize(workspace).await?;
    let mut command = tokio::process::Command::new("git");
    crate::workspace::git::git_command(&mut command)
        .stderr(Stdio::null())
        .arg("-C")
        .arg(&workspace)
        .args(["rev-parse", "--git-common-dir"]);
    let (path, kind) = match git_output(command, GIT_DEADLINE).await? {
        // Git answers relative to the folder it ran in (`.git`, `../.git`) or absolutely.
        Some(common_dir) => (
            tokio::fs::canonicalize(workspace.join(common_dir)).await?,
            AliasKind::GitCommonDir,
        ),
        None => (workspace, AliasKind::Path),
    };
    let alias = path
        .to_str()
        .ok_or_else(|| Error::invalid("the project's path is not UTF-8"))?
        .to_owned();
    let name = project_name(&path, kind).unwrap_or(&alias).to_owned();
    Ok(ProjectIdentity { alias, kind, name })
}

/// The one path `command` prints. `None` when it cannot start or fails: no git on the
/// machine, or a folder outside any repository.
async fn git_output(
    mut command: tokio::process::Command,
    deadline: Duration,
) -> Result<Option<PathBuf>, Error> {
    let Ok(mut child) = command.spawn() else {
        return Ok(None);
    };
    let Some(stdout) = child.stdout.take() else {
        return Ok(None);
    };
    let work = async {
        let mut output = Vec::new();
        stdout
            .take(MAX_GIT_OUTPUT)
            .read_to_end(&mut output)
            .await
            .ok()?;
        child.wait().await.ok()?.success().then_some(output)
    };
    // The child is killed when it is dropped, which the timeout does on expiry.
    let Ok(output) = tokio::time::timeout(deadline, work).await else {
        return Err(Error::invalid(format!(
            "git did not name the repository within {} ms",
            deadline.as_millis()
        )));
    };
    let Some(output) = output else {
        return Ok(None);
    };
    let path = std::str::from_utf8(&output)
        .map_err(|_| Error::invalid("the repository's path is not UTF-8"))?
        .trim_end_matches(['\n', '\r']);
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

/// The name a project takes from its alias: the folder that holds `.git`, a bare repository
/// without its `.git` suffix, or the folder itself.
fn project_name(path: &Path, kind: AliasKind) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    if kind == AliasKind::Path {
        return Some(name);
    }
    if name == ".git" {
        return path.parent()?.file_name()?.to_str();
    }
    Some(name.strip_suffix(".git").unwrap_or(name)).filter(|name| !name.is_empty())
}

/// A project or group name: on one line, not empty, at most [`MAX_NAME_CHARS`] characters.
fn clean_name(name: &str, what: &str) -> Result<String, Error> {
    required(&one_line(name), what, MAX_NAME_CHARS)
}

fn ids(connection: &Connection, sql: &str, id: Option<&str>) -> Result<Vec<String>, Error> {
    let mut statement = connection.prepare(sql)?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(id), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The id of every project.
pub(super) fn all_ids(connection: &Connection) -> Result<Vec<String>, Error> {
    ids(
        connection,
        "SELECT id FROM projects ORDER BY name COLLATE NOCASE, id",
        None,
    )
}

/// The ids of a group's projects.
pub(super) fn member_ids(connection: &Connection, group_id: &str) -> Result<Vec<String>, Error> {
    ids(
        connection,
        "SELECT pg.project_id FROM project_groups pg \
         JOIN projects p ON p.id = pg.project_id \
         WHERE pg.group_id = ?1 ORDER BY p.name COLLATE NOCASE, p.id",
        Some(group_id),
    )
}

pub(super) fn load(connection: &Connection, id: &str) -> Result<Option<Project>, Error> {
    let Some((name, created_at, group_answered)) = connection
        .query_row(
            "SELECT name, created_at, group_answered_at IS NOT NULL FROM projects WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT alias, kind, added_at FROM project_aliases \
         WHERE project_id = ?1 ORDER BY added_at, alias",
    )?;
    let aliases = statement
        .query_map([id], |row| {
            Ok(ProjectAlias {
                alias: row.get(0)?,
                kind: row.get(1)?,
                added_at: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let groups = ids(
        connection,
        "SELECT pg.group_id FROM project_groups pg \
         JOIN memory_groups g ON g.id = pg.group_id \
         WHERE pg.project_id = ?1 AND g.retired_at IS NULL \
         ORDER BY g.name COLLATE NOCASE, g.id",
        Some(id),
    )?;
    Ok(Some(Project {
        id: id.to_owned(),
        name,
        created_at,
        group_answered,
        aliases,
        groups,
    }))
}

pub(super) fn require_project(connection: &Connection, id: &str) -> Result<Project, Error> {
    load(connection, id)?.ok_or_else(|| Error::not_found("project", id))
}

fn load_group(connection: &Connection, id: &str) -> Result<Option<Group>, Error> {
    let Some((name, created_at, retired_at)) = connection
        .query_row(
            "SELECT name, created_at, retired_at FROM memory_groups WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    Ok(Some(Group {
        id: id.to_owned(),
        name,
        created_at,
        retired_at,
        members: member_ids(connection, id)?,
    }))
}

fn require_group(connection: &Connection, id: &str) -> Result<Group, Error> {
    load_group(connection, id)?.ok_or_else(|| Error::not_found("group", id))
}

/// A group that still shares: a retired one keeps its items and its history, and takes no new
/// item, member or name.
pub(super) fn require_live_group(connection: &Connection, id: &str) -> Result<Group, Error> {
    let group = require_group(connection, id)?;
    if group.retired_at.is_some() {
        return Err(Error::invalid(format!(
            "the group {} is retired",
            group.name
        )));
    }
    Ok(group)
}

/// The id of the live group with this name, whatever the case of its ASCII letters.
fn live_group_id(connection: &Connection, name: &str) -> Result<Option<String>, Error> {
    Ok(connection
        .query_row(
            "SELECT id FROM memory_groups \
             WHERE name = ?1 COLLATE NOCASE AND retired_at IS NULL",
            [name],
            |row| row.get(0),
        )
        .optional()?)
}

/// Refuses a name another live group holds; `own` is the group being renamed.
fn check_group_name(connection: &Connection, name: &str, own: Option<&str>) -> Result<(), Error> {
    match live_group_id(connection, name)? {
        Some(holder) if own != Some(holder.as_str()) => Err(Error::invalid(format!(
            "a group named {name} already exists"
        ))),
        _ => Ok(()),
    }
}

const SELECT_CHANGES: &str = "SELECT id, relation, subject, object, change, status, actor, \
     session_id, at, decided_at FROM membership_changes";

fn change_from_row(row: &Row<'_>) -> rusqlite::Result<MembershipChange> {
    let relation: String = row.get(1)?;
    Ok(MembershipChange {
        id: row.get(0)?,
        relation: Relation::from_columns(&relation, row.get(2)?, row.get(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        change: row.get(4)?,
        status: row.get(5)?,
        actor: row.get(6)?,
        session_id: row.get(7)?,
        at: row.get(8)?,
        decided_at: row.get(9)?,
    })
}

fn changes(
    connection: &Connection,
    filter: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<MembershipChange>, Error> {
    let mut statement = connection.prepare(&format!("{SELECT_CHANGES} {filter}"))?;
    let changes = statement
        .query_map(values, change_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(changes)
}

fn require_change(connection: &Connection, id: i64) -> Result<MembershipChange, Error> {
    connection
        .query_row(
            &format!("{SELECT_CHANGES} WHERE id = ?1"),
            [id],
            change_from_row,
        )
        .optional()?
        .ok_or_else(|| Error::not_found("membership change", &id.to_string()))
}

/// Appends one line of membership history. Earlier lines are never rewritten: a project that
/// joined, left and joined again shows three lines, which is how the Captain can tell what a
/// session could read at a given date.
fn record(
    connection: &Connection,
    relation: &Relation,
    change: ChangeKind,
    status: ChangeStatus,
    by: &ChangedBy,
    at: &str,
) -> Result<MembershipChange, Error> {
    let (name, subject, object) = relation.columns();
    let decided_at = (status != ChangeStatus::Proposed).then_some(at);
    connection.execute(
        "INSERT INTO membership_changes \
             (relation, subject, object, change, status, actor, session_id, at, decided_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            name,
            subject,
            object,
            change,
            status,
            by.actor,
            by.session_id,
            at,
            decided_at
        ],
    )?;
    require_change(connection, connection.last_insert_rowid())
}

/// Whether a link can be added: both ends exist, the group still shares, and the link is not
/// already there.
fn check_add(connection: &Connection, relation: &Relation) -> Result<(), Error> {
    match relation {
        Relation::ProjectGroup {
            project_id,
            group_id,
        } => {
            require_project(connection, project_id)?;
            let group = require_live_group(connection, group_id)?;
            if group.members.contains(project_id) {
                return Err(Error::invalid(format!(
                    "the project is already in the group {}",
                    group.name
                )));
            }
        }
        Relation::SessionProject {
            session_id,
            project_id,
        } => {
            if session_id.trim().is_empty() {
                return Err(Error::invalid("the link names no session"));
            }
            let project = require_project(connection, project_id)?;
            if active_project_id(connection, session_id)?.as_deref() == Some(project_id) {
                return Err(Error::invalid(format!(
                    "the session already works in {}",
                    project.name
                )));
            }
        }
    }
    Ok(())
}

/// Adds a link [`check_add`] accepted.
fn apply_add(connection: &Connection, relation: &Relation, at: &str) -> Result<(), Error> {
    match relation {
        Relation::ProjectGroup {
            project_id,
            group_id,
        } => {
            connection.execute(
                "INSERT INTO project_groups (project_id, group_id) VALUES (?1, ?2)",
                [project_id, group_id],
            )?;
            // A project put in a group has its answer to "which group?".
            connection.execute(
                "UPDATE projects SET group_answered_at = COALESCE(group_answered_at, ?2) \
                 WHERE id = ?1",
                [project_id.as_str(), at],
            )?;
        }
        Relation::SessionProject {
            session_id,
            project_id,
        } => {
            // One project is active per session: the earlier link stays, inactive, so the
            // session reads the new project's items and groups and no longer the old one's.
            connection.execute(
                "UPDATE session_projects SET active = 0 WHERE session_id = ?1 AND active = 1",
                [session_id],
            )?;
            connection.execute(
                "INSERT INTO session_projects (session_id, project_id, active) \
                 VALUES (?1, ?2, 1) \
                 ON CONFLICT(session_id, project_id) DO UPDATE SET active = 1",
                [session_id, project_id],
            )?;
        }
    }
    Ok(())
}

fn active_project_id(connection: &Connection, session_id: &str) -> Result<Option<String>, Error> {
    Ok(connection
        .query_row(
            "SELECT project_id FROM session_projects WHERE session_id = ?1 AND active = 1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn links(connection: &Connection, filter: &str, id: &str) -> Result<Vec<SessionLink>, Error> {
    let mut statement = connection.prepare(&format!(
        "SELECT session_id, project_id, active FROM session_projects {filter}"
    ))?;
    let links = statement
        .query_map([id], |row| {
            Ok(SessionLink {
                session_id: row.get(0)?,
                project_id: row.get(1)?,
                active: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(links)
}

impl MemoryStore {
    /// The project an identity belongs to, created with its group question open when the
    /// alias was never seen.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty alias.
    pub fn resolve_project(&self, identity: &ProjectIdentity) -> Result<ResolvedProject, Error> {
        if identity.alias.is_empty() {
            return Err(Error::invalid("the project has no alias"));
        }
        // A label taken from a folder name, which the Captain can change: a long one is cut
        // rather than refusing to know the project.
        let name: String = one_line(&identity.name)
            .chars()
            .take(MAX_NAME_CHARS)
            .collect();
        let name = if name.is_empty() {
            "project".to_owned()
        } else {
            name
        };
        self.write(|transaction| {
            let known: Option<String> = transaction
                .query_row(
                    "SELECT project_id FROM project_aliases WHERE alias = ?1",
                    [&identity.alias],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = known {
                return Ok(ResolvedProject {
                    project: require_project(transaction, &id)?,
                    created: false,
                });
            }
            let (id, at) = (new_id()?, now());
            transaction.execute(
                "INSERT INTO projects (id, name, created_at) VALUES (?1, ?2, ?3)",
                [&id, &name, &at],
            )?;
            transaction.execute(
                "INSERT INTO project_aliases (alias, kind, project_id, added_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![identity.alias, identity.kind, id, at],
            )?;
            Ok(ResolvedProject {
                project: require_project(transaction, &id)?,
                created: true,
            })
        })
    }

    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn project(&self, id: &str) -> Result<Option<Project>, Error> {
        self.read(|connection| load(connection, id))
    }

    /// Every project, by name.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn projects(&self) -> Result<Vec<Project>, Error> {
        self.read(|connection| {
            let mut projects = Vec::new();
            for id in all_ids(connection)? {
                projects.extend(load(connection, &id)?);
            }
            Ok(projects)
        })
    }

    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty or over-long name, [`Error::NotFound`] for an unknown
    /// project.
    pub fn rename_project(&self, id: &str, name: &str) -> Result<Project, Error> {
        let name = clean_name(name, "project name")?;
        self.write(|transaction| {
            transaction.execute("UPDATE projects SET name = ?2 WHERE id = ?1", [id, &name])?;
            require_project(transaction, id)
        })
    }

    /// Records that the Captain answered which group a project belongs to, whatever the
    /// answer: "none" closes the question as joining a group does. Answered once; a later call
    /// keeps the first date.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project.
    pub fn answer_group_question(&self, project_id: &str) -> Result<Project, Error> {
        self.write(|transaction| {
            transaction.execute(
                "UPDATE projects SET group_answered_at = COALESCE(group_answered_at, ?2) \
                 WHERE id = ?1",
                [project_id, &now()],
            )?;
            require_project(transaction, project_id)
        })
    }

    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty or over-long name, or one a live group already holds.
    pub fn create_group(&self, name: &str) -> Result<Group, Error> {
        let name = clean_name(name, "group name")?;
        self.write(|transaction| {
            check_group_name(transaction, &name, None)?;
            let id = new_id()?;
            transaction.execute(
                "INSERT INTO memory_groups (id, name, created_at) VALUES (?1, ?2, ?3)",
                [&id, &name, &now()],
            )?;
            require_group(transaction, &id)
        })
    }

    /// # Errors
    ///
    /// As [`Self::create_group`], and [`Error::NotFound`] for an unknown group,
    /// [`Error::Invalid`] for a retired one.
    pub fn rename_group(&self, id: &str, name: &str) -> Result<Group, Error> {
        let name = clean_name(name, "group name")?;
        self.write(|transaction| {
            require_live_group(transaction, id)?;
            check_group_name(transaction, &name, Some(id))?;
            transaction.execute(
                "UPDATE memory_groups SET name = ?2 WHERE id = ?1",
                [id, &name],
            )?;
            require_group(transaction, id)
        })
    }

    /// Stops a group from sharing: its items leave every session's reading and it takes no
    /// new item or member. The group, its members and its items stay readable, and its name
    /// is free for a new group.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown group, [`Error::Invalid`] for one already retired.
    pub fn retire_group(&self, id: &str) -> Result<Group, Error> {
        self.write(|transaction| {
            require_live_group(transaction, id)?;
            transaction.execute(
                "UPDATE memory_groups SET retired_at = ?2 WHERE id = ?1",
                [id, &now()],
            )?;
            require_group(transaction, id)
        })
    }

    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn group(&self, id: &str) -> Result<Option<Group>, Error> {
        self.read(|connection| load_group(connection, id))
    }

    /// The live group with this name, whatever the case of its ASCII letters.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn group_by_name(&self, name: &str) -> Result<Option<Group>, Error> {
        self.read(
            |connection| match live_group_id(connection, &one_line(name))? {
                Some(id) => load_group(connection, &id),
                None => Ok(None),
            },
        )
    }

    /// Every group, live ones first, by name.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn groups(&self) -> Result<Vec<Group>, Error> {
        self.read(|connection| {
            let mut groups = Vec::new();
            for id in ids(
                connection,
                "SELECT id FROM memory_groups \
                 ORDER BY retired_at IS NOT NULL, name COLLATE NOCASE, id",
                None,
            )? {
                groups.extend(load_group(connection, &id)?);
            }
            Ok(groups)
        })
    }

    /// Puts a project in a group at once. Joining a group lets the project read everything
    /// the group shares, so it is written only when `confirmed` by the Captain.
    ///
    /// # Errors
    ///
    /// [`Error::WideningRequiresConfirmation`] with the group's current members, when not
    /// confirmed; [`Error::NotFound`] for an unknown project or group; [`Error::Invalid`] for
    /// a retired group or a project already in it. Nothing is written then.
    pub fn add_project_to_group(
        &self,
        project_id: &str,
        group_id: &str,
        confirmed: bool,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        let relation = Relation::ProjectGroup {
            project_id: project_id.to_owned(),
            group_id: group_id.to_owned(),
        };
        self.write(|transaction| {
            check_add(transaction, &relation)?;
            if !confirmed {
                let mut affected_projects = Vec::new();
                for id in member_ids(transaction, group_id)? {
                    affected_projects.extend(load(transaction, &id)?);
                }
                return Err(Error::WideningRequiresConfirmation { affected_projects });
            }
            let at = now();
            apply_add(transaction, &relation, &at)?;
            record(
                transaction,
                &relation,
                ChangeKind::Add,
                ChangeStatus::Applied,
                by,
                &at,
            )
        })
    }

    /// Takes a project out of a group at once: a narrowing needs no confirmation.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the project is not in the group.
    pub fn remove_project_from_group(
        &self,
        project_id: &str,
        group_id: &str,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        self.write(|transaction| {
            let removed = transaction.execute(
                "DELETE FROM project_groups WHERE project_id = ?1 AND group_id = ?2",
                [project_id, group_id],
            )?;
            if removed == 0 {
                return Err(Error::invalid("the project is not in the group"));
            }
            record(
                transaction,
                &Relation::ProjectGroup {
                    project_id: project_id.to_owned(),
                    group_id: group_id.to_owned(),
                },
                ChangeKind::Remove,
                ChangeStatus::Applied,
                by,
                &now(),
            )
        })
    }

    /// Asks the Captain to put a project in a group; nothing changes until
    /// [`Self::accept_membership_change`]. Asking again while the question waits answers the
    /// waiting question instead of adding one.
    ///
    /// # Errors
    ///
    /// As [`Self::add_project_to_group`] when confirmed.
    pub fn propose_project_group(
        &self,
        project_id: &str,
        group_id: &str,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        self.propose(
            &Relation::ProjectGroup {
                project_id: project_id.to_owned(),
                group_id: group_id.to_owned(),
            },
            by,
        )
    }

    /// Makes a project the one a session works in, at once. A project the session worked in
    /// before stays linked and inactive.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project, [`Error::Invalid`] when the session already
    /// works in it.
    pub fn attach_session(
        &self,
        session_id: &str,
        project_id: &str,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        let relation = Relation::SessionProject {
            session_id: session_id.to_owned(),
            project_id: project_id.to_owned(),
        };
        self.write(|transaction| {
            check_add(transaction, &relation)?;
            let at = now();
            apply_add(transaction, &relation, &at)?;
            record(
                transaction,
                &relation,
                ChangeKind::Add,
                ChangeStatus::Applied,
                by,
                &at,
            )
        })
    }

    /// Removes the link between a session and the project it works in. The session then reads
    /// its own items and the global ones; what it was already given is not taken back.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the session works in no project.
    pub fn detach_session(
        &self,
        session_id: &str,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        self.write(|transaction| {
            let Some(project_id) = active_project_id(transaction, session_id)? else {
                return Err(Error::invalid("the session works in no project"));
            };
            transaction.execute(
                "DELETE FROM session_projects WHERE session_id = ?1 AND project_id = ?2",
                [session_id, &project_id],
            )?;
            record(
                transaction,
                &Relation::SessionProject {
                    session_id: session_id.to_owned(),
                    project_id,
                },
                ChangeKind::Remove,
                ChangeStatus::Applied,
                by,
                &now(),
            )
        })
    }

    /// Asks the Captain to make a project the one a session works in; see
    /// [`Self::propose_project_group`].
    ///
    /// # Errors
    ///
    /// As [`Self::attach_session`].
    pub fn propose_session_attach(
        &self,
        session_id: &str,
        project_id: &str,
        by: &ChangedBy,
    ) -> Result<MembershipChange, Error> {
        self.propose(
            &Relation::SessionProject {
                session_id: session_id.to_owned(),
                project_id: project_id.to_owned(),
            },
            by,
        )
    }

    fn propose(&self, relation: &Relation, by: &ChangedBy) -> Result<MembershipChange, Error> {
        self.write(|transaction| {
            check_add(transaction, relation)?;
            let (name, subject, object) = relation.columns();
            // An agent that asks at every turn must not fill the Captain's list: a review
            // queue that only grows stops being read.
            let waiting = changes(
                transaction,
                "WHERE relation = ?1 AND subject = ?2 AND object = ?3 \
                 AND change = 'add' AND status = 'proposed' LIMIT 1",
                [name, subject, object],
            )?;
            match waiting.into_iter().next() {
                Some(waiting) => Ok(waiting),
                None => record(
                    transaction,
                    relation,
                    ChangeKind::Add,
                    ChangeStatus::Proposed,
                    by,
                    &now(),
                ),
            }
        })
    }

    /// The Captain accepts a proposed change: it is applied now, as if just asked.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown change; [`Error::Invalid`] for one already decided,
    /// or one that no longer applies (the group was retired, the link exists).
    pub fn accept_membership_change(&self, change_id: i64) -> Result<MembershipChange, Error> {
        self.decide(change_id, ChangeStatus::Applied)
    }

    /// The Captain refuses a proposed change: nothing is linked, and the refusal is kept.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown change, [`Error::Invalid`] for one already decided.
    pub fn reject_membership_change(&self, change_id: i64) -> Result<MembershipChange, Error> {
        self.decide(change_id, ChangeStatus::Rejected)
    }

    fn decide(&self, change_id: i64, status: ChangeStatus) -> Result<MembershipChange, Error> {
        self.write(|transaction| {
            let change = require_change(transaction, change_id)?;
            if change.status != ChangeStatus::Proposed {
                return Err(Error::invalid(format!(
                    "the change was already {}",
                    change.status
                )));
            }
            let at = now();
            if status == ChangeStatus::Applied {
                // Only additions wait for an answer; a removal narrows and applies at once.
                if change.change != ChangeKind::Add {
                    return Err(Error::invalid("a proposed removal cannot be applied"));
                }
                check_add(transaction, &change.relation)?;
                apply_add(transaction, &change.relation, &at)?;
            }
            transaction.execute(
                "UPDATE membership_changes SET status = ?2, decided_at = ?3 WHERE id = ?1",
                params![change_id, status, at],
            )?;
            require_change(transaction, change_id)
        })
    }

    /// The changes waiting for the Captain, oldest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn pending_membership_changes(&self, limit: u32) -> Result<Vec<MembershipChange>, Error> {
        self.read(|connection| {
            changes(
                connection,
                "WHERE status = 'proposed' ORDER BY id LIMIT ?1",
                [limit],
            )
        })
    }

    /// The membership changes, newest first, at most `limit`: all of them, or those that
    /// involve one project (its groups and the sessions that worked in it).
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn membership_history(
        &self,
        project_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MembershipChange>, Error> {
        self.read(|connection| match project_id {
            Some(project_id) => changes(
                connection,
                "WHERE (relation = 'project_group' AND subject = ?1) \
                    OR (relation = 'session_project' AND object = ?1) \
                 ORDER BY id DESC LIMIT ?2",
                params![project_id, limit],
            ),
            None => changes(connection, "ORDER BY id DESC LIMIT ?1", [limit]),
        })
    }

    /// The project a session works in, when it has one.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn session_project(&self, session_id: &str) -> Result<Option<Project>, Error> {
        self.read(
            |connection| match active_project_id(connection, session_id)? {
                Some(id) => load(connection, &id),
                None => Ok(None),
            },
        )
    }

    /// Every project a session is linked to, the active one first.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn session_links(&self, session_id: &str) -> Result<Vec<SessionLink>, Error> {
        self.read(|connection| {
            links(
                connection,
                "WHERE session_id = ?1 ORDER BY active DESC, project_id",
                session_id,
            )
        })
    }

    /// Every session linked to a project, those working in it first.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn project_sessions(&self, project_id: &str) -> Result<Vec<SessionLink>, Error> {
        self.read(|connection| {
            links(
                connection,
                "WHERE project_id = ?1 ORDER BY active DESC, session_id",
                project_id,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_git_gives_no_project_rather_than_the_folder() {
        let mut slow = tokio::process::Command::new("sleep");
        crate::workspace::git::git_command(&mut slow).arg("5");

        let outcome = git_output(slow, Duration::from_millis(50)).await;

        assert!(matches!(outcome, Err(Error::Invalid(_))), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_machine_without_git_makes_every_folder_its_own_project() {
        let mut missing = tokio::process::Command::new("/no/such/git");
        crate::workspace::git::git_command(&mut missing);

        let outcome = git_output(missing, GIT_DEADLINE).await;

        assert!(matches!(outcome, Ok(None)), "{outcome:?}");
    }

    #[test]
    fn a_project_is_named_after_its_repository() {
        fn name(path: &str, kind: AliasKind) -> Option<&str> {
            project_name(Path::new(path), kind)
        }

        assert_eq!(
            name("/work/navai/.git", AliasKind::GitCommonDir),
            Some("navai")
        );
        assert_eq!(
            name("/srv/navai.git", AliasKind::GitCommonDir),
            Some("navai")
        );
        assert_eq!(name("/work/notes", AliasKind::Path), Some("notes"));
        assert_eq!(name("/work/site.git", AliasKind::Path), Some("site.git"));
        assert_eq!(name("/", AliasKind::Path), None);
    }
}
