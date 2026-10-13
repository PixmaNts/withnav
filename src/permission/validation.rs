//! One-time Captain validation of skills and flows, bound to a hash of their content.

use super::store::{Error, PermissionStore};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ValidationKind {
    Skill,
    Flow,
}
impl ValidationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Flow => "flow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    pub id: i64,
    pub kind: ValidationKind,
    pub name: String,
    pub location: String,
    pub content_sha256: String,
    pub validated_at: String,
    pub revoked_at: Option<String>,
}
fn read_validation(row: &Row<'_>) -> rusqlite::Result<Validation> {
    let kind: String = row.get(1)?;
    Ok(Validation {
        id: row.get(0)?,
        kind: match kind.as_str() {
            "skill" => ValidationKind::Skill,
            "flow" => ValidationKind::Flow,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        name: row.get(2)?,
        location: row.get(3)?,
        content_sha256: row.get(4)?,
        validated_at: row.get(5)?,
        revoked_at: row.get(6)?,
    })
}
impl PermissionStore {
    pub fn validate(
        &self,
        kind: ValidationKind,
        name: &str,
        location: &str,
        hash: &str,
    ) -> Result<Validation, Error> {
        if name.is_empty()
            || location.is_empty()
            || hash
                .strip_prefix("sha256:")
                .is_none_or(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::Invalid(
                "validation requires a name, location and sha256 digest".into(),
            ));
        }
        let db = self.db();
        if let Some(existing) = db.query_row(
            "SELECT id,kind,name,location,content_sha256,validated_at,revoked_at FROM validations
             WHERE kind=?1 AND location=?2 AND content_sha256=?3 AND revoked_at IS NULL ORDER BY id DESC LIMIT 1",
            params![kind.as_str(), location, hash], read_validation,
        ).optional()? {
            return Ok(existing);
        }
        db.execute(
            "INSERT INTO validations(kind,name,location,content_sha256,validated_at) VALUES (?1,?2,?3,?4,?5)",
            params![kind.as_str(), name, location, hash, crate::bus::event::now_rfc3339()],
        )?;
        Ok(db.query_row(
            "SELECT id,kind,name,location,content_sha256,validated_at,revoked_at FROM validations WHERE id=?1",
            [db.last_insert_rowid()], read_validation,
        )?)
    }
    pub fn revoke_validation(&self, id: i64) -> Result<bool, Error> {
        Ok(self.db().execute(
            "UPDATE validations SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
            params![crate::bus::event::now_rfc3339(), id],
        )? != 0)
    }
    pub fn is_valid(
        &self,
        kind: ValidationKind,
        location: &str,
        hash: &str,
    ) -> Result<bool, Error> {
        Ok(self.db().query_row(
            "SELECT EXISTS(SELECT 1 FROM validations WHERE kind=?1 AND location=?2 AND content_sha256=?3 AND revoked_at IS NULL)",
            params![kind.as_str(), location, hash], |row| row.get(0))?)
    }
    pub fn list_validations(&self) -> Result<Vec<Validation>, Error> {
        let db = self.db();
        let mut statement = db.prepare("SELECT id,kind,name,location,content_sha256,validated_at,revoked_at FROM validations ORDER BY id DESC")?;
        let rows = statement.query_map([], read_validation)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
/// Inspect without creating the database or migrating its schema.
pub fn inspect_valid(
    data: Option<&std::path::Path>,
    kind: ValidationKind,
    location: &str,
    hash: &str,
) -> Result<bool, Error> {
    let Some(path) = data
        .map(|data| data.join("withnav.db"))
        .filter(|path| path.exists())
    else {
        return Ok(false);
    };
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='validations')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    PermissionStore {
        connection: std::sync::Mutex::new(connection),
    }
    .is_valid(kind, location, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_is_bound_to_kind_location_hash_and_revoked_without_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let store = PermissionStore::open(dir.path()).unwrap();
        let hash = format!("sha256:{}", "a".repeat(64));
        assert!(store.list_validations().unwrap().is_empty());
        let approval = store
            .validate(ValidationKind::Flow, "guard", "/flows/guard.toml", &hash)
            .unwrap();
        assert!(inspect_valid(
            Some(dir.path()),
            ValidationKind::Flow,
            &approval.location,
            &hash
        )
        .unwrap());
        assert!(!store
            .is_valid(ValidationKind::Skill, &approval.location, &hash)
            .unwrap());
        assert!(!store
            .is_valid(ValidationKind::Flow, "/other/guard.toml", &hash)
            .unwrap());
        assert!(!store
            .is_valid(
                ValidationKind::Flow,
                &approval.location,
                &format!("sha256:{}", "b".repeat(64))
            )
            .unwrap());
        assert!(store.revoke_validation(approval.id).unwrap());
        assert!(!store.revoke_validation(approval.id).unwrap());
        assert!(!store
            .is_valid(ValidationKind::Flow, &approval.location, &hash)
            .unwrap());
        assert!(store.list_validations().unwrap()[0].revoked_at.is_some());
        drop(store);
        assert_eq!(
            PermissionStore::open(dir.path())
                .unwrap()
                .list_validations()
                .unwrap()
                .len(),
            1
        );
    }
}
