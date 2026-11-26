//! Memory items and their append-only versions.

use std::fmt::Write as _;

use rusqlite::{Connection, OptionalExtension, Row, params, types::Value};

use super::{
    projects,
    store::{Actor, Error, MemoryStore, bounded, new_id, now, one_line, required},
};

/// The longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
/// The longest body, in characters.
pub const MAX_BODY_CHARS: usize = 4000;
/// The longest explanation of why an item holds, in characters.
pub const MAX_WHY_CHARS: usize = 2000;
/// The longest source quote, in characters.
pub const MAX_QUOTE_CHARS: usize = 4000;
/// The longest reason given for a change, in characters.
pub const MAX_REASON_CHARS: usize = 500;
/// The shortest beginning of an identifier that may stand for it.
pub const MIN_ID_PREFIX_CHARS: usize = 6;

/// What an item is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Decision,
    Convention,
    Preference,
    Environment,
    Pitfall,
}

text_enum!(Kind, "kind", {
    Decision => "decision",
    Convention => "convention",
    Preference => "preference",
    Environment => "environment",
    Pitfall => "pitfall",
});

/// The breadth of a scope, without what it points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    Global,
    Group,
    Project,
    Session,
}

text_enum!(ScopeKind, "scope", {
    Global => "global",
    Group => "group",
    Project => "project",
    Session => "session",
});

/// Who reads an item: every session, the sessions working in a group's projects (by group
/// id), those working in one project (by project id), or one session (by session id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Group(String),
    Project(String),
    Session(String),
}

impl Scope {
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        match self {
            Self::Global => ScopeKind::Global,
            Self::Group(_) => ScopeKind::Group,
            Self::Project(_) => ScopeKind::Project,
            Self::Session(_) => ScopeKind::Session,
        }
    }

    /// The id the scope points to; empty for global.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::Global => "",
            Self::Group(id) | Self::Project(id) | Self::Session(id) => id,
        }
    }

    /// The scope of a kind and the id it points to.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a scope other than global points to nothing.
    pub fn from_parts(kind: ScopeKind, reference: &str) -> Result<Self, Error> {
        if kind != ScopeKind::Global && reference.trim().is_empty() {
            return Err(Error::invalid(format!("a {kind} scope names no {kind}")));
        }
        let reference = reference.to_owned();
        Ok(match kind {
            ScopeKind::Global => Self::Global,
            ScopeKind::Group => Self::Group(reference),
            ScopeKind::Project => Self::Project(reference),
            ScopeKind::Session => Self::Session(reference),
        })
    }
}

/// Where an item stands. "Superseded" is not a status: it is any version below the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemStatus {
    Active,
    /// Drafted and waiting for the Captain to accept its wording and scope.
    Proposed,
    Invalidated,
    Rejected,
}

text_enum!(ItemStatus, "status", {
    Active => "active",
    Proposed => "proposed",
    Invalidated => "invalidated",
    Rejected => "rejected",
});

/// What a version did to its item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionChange {
    Created,
    Updated,
    Invalidated,
    Restored,
    Rescoped,
}

text_enum!(VersionChange, "change", {
    Created => "created",
    Updated => "updated",
    Invalidated => "invalidated",
    Restored => "restored",
    Rescoped => "rescoped",
});

/// Who is behind a version, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Whose words the content is.
    pub source_actor: Actor,
    /// Who put them in this form.
    pub drafted_by: Actor,
    /// Who accepted this wording and scope, when someone did.
    pub confirmed_by: Option<Actor>,
    pub session_id: Option<String>,
    /// The sequence number of the session event the content came from.
    pub event_seq: Option<u64>,
}

impl Provenance {
    /// The Captain's own words, written and confirmed by the Captain.
    #[must_use]
    pub const fn captain(session_id: Option<String>, event_seq: Option<u64>) -> Self {
        Self {
            source_actor: Actor::Captain,
            drafted_by: Actor::Captain,
            confirmed_by: Some(Actor::Captain),
            session_id,
            event_seq,
        }
    }
}

/// One dated state of an item. Every version holds the whole content, so any of them reads
/// alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryVersion {
    pub version: u32,
    pub change: VersionChange,
    pub title: String,
    pub body: String,
    pub why: Option<String>,
    pub quote: Option<String>,
    pub scope: Scope,
    pub source_actor: Actor,
    pub drafted_by: Actor,
    pub confirmed_by: Option<Actor>,
    pub source_session_id: Option<String>,
    pub source_event_seq: Option<u64>,
    /// Why this change was made, when its author said.
    pub reason: Option<String>,
    pub at: String,
}

/// An item with its current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    pub id: String,
    pub kind: Kind,
    pub scope: Scope,
    pub status: ItemStatus,
    pub current_version: u32,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
    pub current: MemoryVersion,
}

/// What to remember.
#[derive(Debug, Clone)]
pub struct NewItem {
    pub kind: Kind,
    pub scope: Scope,
    /// `None` or blank: the first line of the body, cut at [`MAX_TITLE_CHARS`].
    pub title: Option<String>,
    pub body: String,
    pub why: Option<String>,
    pub quote: Option<String>,
    pub by: Provenance,
}

/// A new wording. `None` keeps what the current version says; an empty `why` removes it.
#[derive(Debug, Clone, Default)]
pub struct Revision {
    pub title: Option<String>,
    pub body: Option<String>,
    pub why: Option<String>,
}

/// Which items to list. Every field narrows; the default lists everything.
#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    /// Only what this session may read.
    pub eligible_for: Option<String>,
    pub scope: Option<ScopeKind>,
    pub scope_ref: Option<String>,
    pub status: Option<ItemStatus>,
    pub kind: Option<Kind>,
    /// Only items whose current title, body or quote contains this text, whatever the case.
    pub text: Option<String>,
    pub limit: Option<u32>,
}

/// The columns of one version, as every query selects them from `memory_versions v`.
macro_rules! version_columns {
    () => {
        "v.version, v.change, v.title, v.body, v.why, v.quote, v.scope, v.scope_ref, \
         v.source_actor, v.drafted_by, v.confirmed_by, v.source_session_id, \
         v.source_event_seq, v.reason, v.at"
    };
}

/// Items with their current version. Nine item columns, then the version's.
const SELECT_ITEMS: &str = concat!(
    "SELECT i.id, i.kind, i.scope, i.scope_ref, i.status, i.current_version, i.pinned, \
     i.created_at, i.updated_at, ",
    version_columns!(),
    " FROM memory_items i \
     JOIN memory_versions v ON v.item_id = i.id AND v.version = i.current_version"
);
const ITEM_COLUMNS: usize = 9;

const SELECT_VERSIONS: &str = concat!(
    "SELECT ",
    version_columns!(),
    " FROM memory_versions v WHERE v.item_id = ?1"
);

/// What a session may read, for the session id bound at `?{n}`: its own items, those of its ONE
/// active project, those of that project's live groups, and the global ones. A project the
/// session is linked to but not working in brings nothing, and neither do its groups: a
/// session that moved to another project must stop reading the first one's shared knowledge.
fn eligible_clause(n: usize) -> String {
    format!(
        " AND (i.scope = 'global' \
           OR (i.scope = 'session' AND i.scope_ref = ?{n}) \
           OR (i.scope = 'project' AND i.scope_ref IN ( \
                 SELECT project_id FROM session_projects \
                 WHERE session_id = ?{n} AND active = 1)) \
           OR (i.scope = 'group' AND i.scope_ref IN ( \
                 SELECT pg.group_id FROM project_groups pg \
                 JOIN session_projects sp ON sp.project_id = pg.project_id \
                 JOIN memory_groups g ON g.id = pg.group_id \
                 WHERE sp.session_id = ?{n} AND sp.active = 1 AND g.retired_at IS NULL)))"
    )
}

fn scope_from_row(row: &Row<'_>, at: usize) -> rusqlite::Result<Scope> {
    let kind: ScopeKind = row.get(at)?;
    let reference: String = row.get(at + 1)?;
    Scope::from_parts(kind, &reference).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            at + 1,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

/// The version whose columns start at `at`.
fn version_from_row(row: &Row<'_>, at: usize) -> rusqlite::Result<MemoryVersion> {
    Ok(MemoryVersion {
        version: row.get(at)?,
        change: row.get(at + 1)?,
        title: row.get(at + 2)?,
        body: row.get(at + 3)?,
        why: row.get(at + 4)?,
        quote: row.get(at + 5)?,
        scope: scope_from_row(row, at + 6)?,
        source_actor: row.get(at + 8)?,
        drafted_by: row.get(at + 9)?,
        confirmed_by: row.get(at + 10)?,
        source_session_id: row.get(at + 11)?,
        source_event_seq: row
            .get::<_, Option<i64>>(at + 12)?
            .map(u64::try_from)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    at + 12,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?,
        reason: row.get(at + 13)?,
        at: row.get(at + 14)?,
    })
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryItem> {
    Ok(MemoryItem {
        id: row.get(0)?,
        kind: row.get(1)?,
        scope: scope_from_row(row, 2)?,
        status: row.get(4)?,
        current_version: row.get(5)?,
        pinned: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        current: version_from_row(row, ITEM_COLUMNS)?,
    })
}

fn load(connection: &Connection, id: &str) -> Result<Option<MemoryItem>, Error> {
    Ok(connection
        .query_row(
            &format!("{SELECT_ITEMS} WHERE i.id = ?1"),
            [id],
            item_from_row,
        )
        .optional()?)
}

fn require(connection: &Connection, id: &str) -> Result<MemoryItem, Error> {
    load(connection, id)?.ok_or_else(|| Error::not_found("memory item", id))
}

/// What a version says, apart from who wrote it and where it applies.
struct Content {
    title: String,
    body: String,
    why: Option<String>,
    quote: Option<String>,
}

impl From<MemoryVersion> for Content {
    fn from(version: MemoryVersion) -> Self {
        Self {
            title: version.title,
            body: version.body,
            why: version.why,
            quote: version.quote,
        }
    }
}

/// The next state of an item, decided from its current one.
struct Amendment {
    change: VersionChange,
    status: ItemStatus,
    scope: Scope,
    content: Content,
}

/// An optional text: absent when it says nothing.
fn optional(text: Option<&str>, what: &str, max: usize) -> Result<Option<String>, Error> {
    let Some(text) = text else { return Ok(None) };
    Ok(Some(bounded(text, what, max)?).filter(|text| !text.is_empty()))
}

/// The title of an item nobody titled: the first line of its body, cut at
/// [`MAX_TITLE_CHARS`]. A label for lists, not the stored text: the body keeps every character.
fn derived_title(body: &str) -> String {
    one_line(body.lines().next().unwrap_or_default())
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect()
}

/// A scope an item can be put in: its project or its group exists, and the group still shares.
fn check_scope(connection: &Connection, scope: &Scope) -> Result<(), Error> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Group(id) => projects::require_live_group(connection, id).map(drop),
        Scope::Project(id) => projects::require_project(connection, id).map(drop),
        Scope::Session(id) if id.trim().is_empty() => {
            Err(Error::invalid("a session scope names no session"))
        }
        Scope::Session(_) => Ok(()),
    }
}

/// The ids of the projects whose sessions read a scope; `None` when every project does. A
/// session scope is read by no project, only by its one session.
fn readers(connection: &Connection, scope: &Scope) -> Result<Option<Vec<String>>, Error> {
    Ok(match scope {
        Scope::Global => None,
        Scope::Group(id) => Some(projects::member_ids(connection, id)?),
        Scope::Project(id) => Some(vec![id.clone()]),
        Scope::Session(_) => Some(Vec::new()),
    })
}

/// Whether moving an item `from` a scope `to` another lets projects read it that could not:
/// `Some` with those projects when it does. A move to global always does, even when no other
/// project exists yet, because every later project will read it. A move to one session never
/// does: one conversation the Captain named is the narrowest there is.
fn widening(
    connection: &Connection,
    from: &Scope,
    to: &Scope,
) -> Result<Option<Vec<projects::Project>>, Error> {
    if matches!(to, Scope::Session(_)) {
        return Ok(None);
    }
    let Some(before) = readers(connection, from)? else {
        return Ok(None);
    };
    let after = match readers(connection, to)? {
        Some(after) => after,
        None => projects::all_ids(connection)?,
    };
    let mut newly = Vec::new();
    for id in after.iter().filter(|id| !before.contains(id)) {
        newly.extend(projects::load(connection, id)?);
    }
    Ok((*to == Scope::Global || !newly.is_empty()).then_some(newly))
}

fn insert_version(
    connection: &Connection,
    id: &str,
    version: u32,
    amendment: &Amendment,
    by: &Provenance,
    reason: Option<&str>,
    at: &str,
) -> Result<(), Error> {
    let event_seq = by
        .event_seq
        .map(i64::try_from)
        .transpose()
        .map_err(|_| Error::invalid("the source event number is out of range"))?;
    let Amendment {
        change,
        scope,
        content,
        ..
    } = amendment;
    connection.execute(
        "INSERT INTO memory_versions (item_id, version, change, title, body, why, quote, \
             scope, scope_ref, source_actor, drafted_by, confirmed_by, source_session_id, \
             source_event_seq, reason, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            id,
            version,
            change,
            content.title,
            content.body,
            content.why,
            content.quote,
            scope.kind(),
            scope.reference(),
            by.source_actor,
            by.drafted_by,
            by.confirmed_by,
            by.session_id,
            event_seq,
            reason,
            at,
        ],
    )?;
    Ok(())
}

/// An item only its current, active state can be changed this way.
fn require_active(item: &MemoryItem, verb: &str) -> Result<(), Error> {
    if item.status == ItemStatus::Active {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "the item is {}: it cannot be {verb}",
        item.status
    )))
}

impl MemoryStore {
    /// Records a new item, active at once, as version 1.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty body or a text over its limit, [`Error::NotFound`] for
    /// a scope that names an unknown project or group. Nothing is stored then.
    pub fn remember(&self, new: NewItem) -> Result<MemoryItem, Error> {
        let body = required(&new.body, "body", MAX_BODY_CHARS)?;
        let title = match new.title.as_deref().map(one_line) {
            Some(title) if !title.is_empty() => bounded(&title, "title", MAX_TITLE_CHARS)?,
            _ => derived_title(&body),
        };
        let amendment = Amendment {
            change: VersionChange::Created,
            status: ItemStatus::Active,
            scope: new.scope,
            content: Content {
                title,
                body,
                why: optional(new.why.as_deref(), "reason why", MAX_WHY_CHARS)?,
                quote: optional(new.quote.as_deref(), "quote", MAX_QUOTE_CHARS)?,
            },
        };
        self.write(|transaction| {
            check_scope(transaction, &amendment.scope)?;
            let (id, at) = (new_id()?, now());
            transaction.execute(
                "INSERT INTO memory_items (id, kind, scope, scope_ref, status, \
                     current_version, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
                params![
                    id,
                    new.kind,
                    amendment.scope.kind(),
                    amendment.scope.reference(),
                    amendment.status,
                    at
                ],
            )?;
            insert_version(transaction, &id, 1, &amendment, &new.by, None, &at)?;
            require(transaction, &id)
        })
    }

    /// Appends the version `next` decides from the item's current state, when the caller read
    /// the current version. The check and the write are one immediate transaction, so of two
    /// writers that read the same version exactly one succeeds.
    fn amend(
        &self,
        id: &str,
        expected_version: impl Into<Option<u32>>,
        reason: Option<&str>,
        by: &Provenance,
        next: impl FnOnce(&Connection, MemoryItem) -> Result<Amendment, Error>,
    ) -> Result<MemoryItem, Error> {
        let expected_version = expected_version.into();
        let reason = optional(reason, "reason", MAX_REASON_CHARS)?;
        self.write(|transaction| {
            let item = require(transaction, id)?;
            if expected_version.is_some_and(|expected| item.current_version != expected) {
                return Err(Error::VersionConflict {
                    current: item.current_version,
                });
            }
            let version = item
                .current_version
                .checked_add(1)
                .ok_or_else(|| Error::invalid("the item has too many versions"))?;
            let amendment = next(transaction, item)?;
            let at = now();
            insert_version(
                transaction,
                id,
                version,
                &amendment,
                by,
                reason.as_deref(),
                &at,
            )?;
            transaction.execute(
                "UPDATE memory_items SET scope = ?2, scope_ref = ?3, status = ?4, \
                     current_version = ?5, updated_at = ?6 WHERE id = ?1",
                params![
                    id,
                    amendment.scope.kind(),
                    amendment.scope.reference(),
                    amendment.status,
                    version,
                    at
                ],
            )?;
            require(transaction, id)
        })
    }

    /// Rewords an active item as a new version. What `revision` leaves out is kept, and so is
    /// the source quote: the item still comes from those words. A title that was only the
    /// body's first line follows a new body.
    ///
    /// # Errors
    ///
    /// [`Error::VersionConflict`] when `expected_version` is not the current one,
    /// [`Error::NotFound`] for an unknown id, [`Error::Invalid`] for an empty revision, a text
    /// over its limit or an item that is not active. Nothing is written then.
    pub fn revise(
        &self,
        id: &str,
        expected_version: u32,
        revision: Revision,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        let title = revision
            .title
            .map(|title| required(&one_line(&title), "title", MAX_TITLE_CHARS))
            .transpose()?;
        let body = revision
            .body
            .map(|body| required(&body, "body", MAX_BODY_CHARS))
            .transpose()?;
        let why = revision
            .why
            .map(|why| optional(Some(&why), "reason why", MAX_WHY_CHARS))
            .transpose()?;
        if title.is_none() && body.is_none() && why.is_none() {
            return Err(Error::invalid("the revision changes nothing"));
        }
        self.amend(id, expected_version, reason, by, |_, item| {
            require_active(&item, "revised")?;
            let current = item.current;
            // A title nobody wrote follows the body: left as it was, it would keep the old
            // wording readable and searchable on the current version.
            let title = match (title, &body) {
                (Some(title), _) => title,
                (None, Some(body)) if current.title == derived_title(&current.body) => {
                    derived_title(body)
                }
                (None, _) => current.title,
            };
            Ok(Amendment {
                change: VersionChange::Updated,
                status: ItemStatus::Active,
                scope: item.scope,
                content: Content {
                    title,
                    body: body.unwrap_or(current.body),
                    why: why.unwrap_or(current.why),
                    quote: current.quote,
                },
            })
        })
    }

    /// Retires an active item: it leaves every session's reading and the search, and keeps its
    /// history. The version added repeats the content, so the item still reads alone.
    ///
    /// # Errors
    ///
    /// As [`Self::revise`].
    pub fn invalidate(
        &self,
        id: &str,
        expected_version: u32,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        self.invalidate_at(id, Some(expected_version), reason, by)
    }

    /// Invalidates whichever version is current in the write transaction.
    pub fn invalidate_current(
        &self,
        id: &str,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        self.invalidate_at(id, None, reason, by)
    }

    fn invalidate_at(
        &self,
        id: &str,
        expected_version: Option<u32>,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        self.amend(id, expected_version, reason, by, |_, item| {
            require_active(&item, "invalidated")?;
            Ok(Amendment {
                change: VersionChange::Invalidated,
                status: ItemStatus::Invalidated,
                scope: item.scope,
                content: item.current.into(),
            })
        })
    }

    /// Makes an item active again as a new version: an invalidated item with what it said, or,
    /// with `from_version`, any item with the wording of that earlier version (the undo of a
    /// revision). The scope stays the current one; only [`Self::rescope`] moves an item.
    ///
    /// # Errors
    ///
    /// [`Error::VersionConflict`] when `expected_version` is not the current one,
    /// [`Error::NotFound`] for an unknown id or `from_version`, [`Error::Invalid`] for an
    /// active item with no `from_version` or an item that was never active.
    pub fn restore(
        &self,
        id: &str,
        expected_version: u32,
        from_version: Option<u32>,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        self.amend(id, expected_version, reason, by, |connection, item| {
            let content = match (item.status, from_version) {
                (ItemStatus::Proposed | ItemStatus::Rejected, _) | (ItemStatus::Active, None) => {
                    return Err(Error::invalid(format!(
                        "the item is {}: there is nothing to restore",
                        item.status
                    )));
                }
                (ItemStatus::Invalidated, None) => item.current.into(),
                (ItemStatus::Active | ItemStatus::Invalidated, Some(version)) => connection
                    .query_row(
                        &format!("{SELECT_VERSIONS} AND v.version = ?2"),
                        params![id, version],
                        |row| version_from_row(row, 0),
                    )
                    .optional()?
                    .ok_or_else(|| Error::NotFound {
                        what: "version",
                        id: format!("{version} of memory item {id}"),
                    })?
                    .into(),
            };
            Ok(Amendment {
                change: VersionChange::Restored,
                status: ItemStatus::Active,
                scope: item.scope,
                content,
            })
        })
    }

    /// Moves an active item to another scope as a new version. A move that lets more projects
    /// read it is written only when `confirmed`: the Captain's own confirmation, never the
    /// agent's.
    ///
    /// # Errors
    ///
    /// [`Error::WideningRequiresConfirmation`] with the projects that would newly read the
    /// item, when the move widens and is not confirmed; otherwise as [`Self::revise`], with
    /// [`Error::NotFound`] for a scope that names an unknown project or group.
    pub fn rescope(
        &self,
        id: &str,
        expected_version: u32,
        to: Scope,
        confirmed: bool,
        reason: Option<&str>,
        by: &Provenance,
    ) -> Result<MemoryItem, Error> {
        self.amend(id, expected_version, reason, by, |connection, item| {
            require_active(&item, "moved")?;
            if item.scope == to {
                return Err(Error::invalid("the item is already in that scope"));
            }
            check_scope(connection, &to)?;
            if !confirmed && let Some(affected_projects) = widening(connection, &item.scope, &to)? {
                return Err(Error::WideningRequiresConfirmation { affected_projects });
            }
            Ok(Amendment {
                change: VersionChange::Rescoped,
                status: ItemStatus::Active,
                scope: to,
                content: item.current.into(),
            })
        })
    }

    /// The item with its current version, whatever its status.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn item(&self, id: &str) -> Result<Option<MemoryItem>, Error> {
        self.read(|connection| load(connection, id))
    }

    /// Every version of an item, oldest first; empty for an unknown id.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn versions(&self, id: &str) -> Result<Vec<MemoryVersion>, Error> {
        self.read(|connection| {
            let mut statement =
                connection.prepare(&format!("{SELECT_VERSIONS} ORDER BY v.version"))?;
            let versions = statement
                .query_map([id], |row| version_from_row(row, 0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(versions)
        })
    }

    /// The id that `prefix` is, or is the only one to begin with.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for fewer than [`MIN_ID_PREFIX_CHARS`] characters or a beginning
    /// several items share, [`Error::NotFound`] when no item matches.
    pub fn resolve_id(&self, prefix: &str) -> Result<String, Error> {
        let prefix = prefix.trim().to_ascii_lowercase();
        if prefix.chars().count() < MIN_ID_PREFIX_CHARS {
            return Err(Error::invalid(format!(
                "an item id needs at least {MIN_ID_PREFIX_CHARS} characters"
            )));
        }
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM memory_items WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2",
            )?;
            let mut ids = statement
                .query_map([&prefix], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match (ids.pop(), ids.is_empty()) {
                (Some(id), true) => Ok(id),
                (Some(_), false) => {
                    Err(Error::invalid(format!("several items begin with {prefix}")))
                }
                (None, _) => Err(Error::not_found("memory item", &prefix)),
            }
        })
    }

    /// The items `filter` keeps, most recently changed first.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn list(&self, filter: &ItemFilter) -> Result<Vec<MemoryItem>, Error> {
        let mut sql = format!("{SELECT_ITEMS} WHERE 1 = 1");
        let mut values: Vec<Value> = Vec::new();
        let mut bind = |sql: &mut String, clause: &str, value: &str| {
            values.push(Value::Text(value.to_owned()));
            // Writing to a String cannot fail.
            let _ = write!(sql, " AND {clause} = ?{}", values.len());
        };
        if let Some(scope) = filter.scope {
            bind(&mut sql, "i.scope", scope.as_str());
        }
        if let Some(reference) = &filter.scope_ref {
            bind(&mut sql, "i.scope_ref", reference);
        }
        if let Some(status) = filter.status {
            bind(&mut sql, "i.status", status.as_str());
        }
        if let Some(kind) = filter.kind {
            bind(&mut sql, "i.kind", kind.as_str());
        }
        if let Some(session_id) = &filter.eligible_for {
            values.push(Value::Text(session_id.clone()));
            sql.push_str(&eligible_clause(values.len()));
        }
        sql.push_str(" ORDER BY i.updated_at DESC, i.rowid DESC");
        // Matched here, not with LIKE: SQLite folds case for ASCII only and the Captain writes
        // French ("Décision"), and a text matched as written needs no wildcard escaping. The
        // register is small; a full-text index is a later step.
        let needle = filter
            .text
            .as_deref()
            .map(|text| text.trim().to_lowercase())
            .filter(|needle| !needle.is_empty());
        let limit = filter.limit.map_or(usize::MAX, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        });
        self.read(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut items = Vec::new();
            for item in statement.query_map(rusqlite::params_from_iter(values), item_from_row)? {
                if items.len() >= limit {
                    break;
                }
                let item = item?;
                let version = &item.current;
                let texts = [
                    Some(&version.title),
                    Some(&version.body),
                    version.quote.as_ref(),
                ];
                if needle.as_ref().is_none_or(|needle| {
                    texts
                        .into_iter()
                        .flatten()
                        .any(|text| text.to_lowercase().contains(needle))
                }) {
                    items.push(item);
                }
            }
            Ok(items)
        })
    }

    /// What a session may read now: the active items of its own scope, of its one active
    /// project, of that project's groups, and the global ones.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub fn eligible(&self, session_id: &str) -> Result<Vec<MemoryItem>, Error> {
        self.list(&ItemFilter {
            eligible_for: Some(session_id.to_owned()),
            status: Some(ItemStatus::Active),
            ..ItemFilter::default()
        })
    }

    /// The active items whose current title, body or quote contains `text`, whatever the case;
    /// with a session, only among what it may read. Earlier versions and invalidated items are
    /// never searched: a superseded wording must not come back as an answer.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a blank `text`.
    pub fn search(&self, text: &str, session_id: Option<&str>) -> Result<Vec<MemoryItem>, Error> {
        if text.trim().is_empty() {
            return Err(Error::invalid("the search text is empty"));
        }
        self.list(&ItemFilter {
            eligible_for: session_id.map(str::to_owned),
            status: Some(ItemStatus::Active),
            text: Some(text.to_owned()),
            ..ItemFilter::default()
        })
    }
}
