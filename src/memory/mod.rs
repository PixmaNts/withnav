//! The memory register: what the Captain asked WithNav to remember, kept in `withnav.db`.
//!
//! A **memory item** is one durable piece of knowledge (a decision, a convention, a preference,
//! an environment fact, a pitfall) with a scope that says who reads it: every session (global),
//! the sessions of a group's projects, those of one project, or one session. An item is never
//! rewritten: each change (a new wording, an invalidation, a restoration, a move to another
//! scope) appends a dated version that names its author and source session, and a writer states
//! the version it read, so two sessions cannot silently supersede each other.
//!
//! A **project** is one repository whatever the checkout: linked git worktrees share it, a
//! separate clone or a folder outside git is its own. A **group** shares knowledge between some
//! projects without making it global. A session works in one active project at a time.
//!
//! Scope decides who reads an item, never what it may do. This module imports nothing of the
//! permission model, the flows or the agent, and the permission path never reads it: a
//! remembered sentence can inform an answer, it can never allow a tool call. The Captain's rule
//! (2026-11-25) is that memory never authorizes; a register readable by the permission path
//! would let a sentence written in one session grant something in another.
//!
//! The store keeps text as it is given. Its callers redact secrets before storing, as they do
//! for any Captain text.

/// The text form of an enum as the database keeps it: `as_str`, `Display` and `FromStr`.
macro_rules! text_enum {
    ($name:ident, $what:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::memory::Error;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    _ => Err($crate::memory::Error::Invalid(format!(
                        concat!("unknown ", $what, ": {}"),
                        text
                    ))),
                }
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                value
                    .as_str()?
                    .parse()
                    .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}

mod items;
mod projects;
mod store;

pub use items::{
    ItemFilter, ItemStatus, Kind, MAX_BODY_CHARS, MAX_QUOTE_CHARS, MAX_REASON_CHARS,
    MAX_TITLE_CHARS, MAX_WHY_CHARS, MIN_ID_PREFIX_CHARS, MemoryItem, MemoryVersion, NewItem,
    Provenance, Revision, Scope, ScopeKind, VersionChange,
};
pub use projects::{
    AliasKind, ChangeKind, ChangeStatus, ChangedBy, Group, MAX_NAME_CHARS, MembershipChange,
    Project, ProjectAlias, ProjectIdentity, Relation, ResolvedProject, SessionLink, identity,
};
pub use store::{Actor, Error, MemoryStore};

#[cfg(test)]
mod tests {
    /// The register must stay unreadable from the permission path and must not reach into it.
    /// A `use` of one of these modules here is the first step of memory deciding a tool call.
    #[test]
    fn the_register_imports_nothing_of_permissions_flows_or_the_agent() {
        let sources = [
            include_str!("mod.rs"),
            include_str!("store.rs"),
            include_str!("items.rs"),
            include_str!("projects.rs"),
        ];
        let forbidden = ["agent", "permission", "flows"].map(|module| format!("crate::{module}"));

        for source in sources {
            let code = source.split("#[cfg(test)]").next().unwrap_or_default();
            for module in &forbidden {
                assert!(!code.contains(module.as_str()), "imports {module}");
            }
        }
    }
}
