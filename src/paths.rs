//! Where the navigator keeps its data, and the one-time move from the
//! former location.
//!
//! Data (the conversation database and the decision log) lives in the XDG
//! data dir, `~/.local/share/navigator` on Linux, the way OpenCode keeps its
//! own. Configuration (models.json, auth.json, the user pipeline dir) stays
//! in the config dir, see [`crate::provider::auth::default_config_dir`].

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The navigator's data dir (`~/.local/share/navigator` on Linux), when the
/// home directory can be resolved.
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "navigator").map(|dirs| dirs.data_dir().to_path_buf())
}

/// The conversation-history database, `<data_dir>/navigator.db`.
pub fn history_db() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("navigator.db"))
}

/// The decision log the replay reads, `<data_dir>/navigator.jsonl`.
pub fn decision_log() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("navigator.jsonl"))
}

/// Where the data lived before it moved to [`data_dir`]: the former
/// `rust-agent` data dir.
fn legacy_data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "rust-agent").map(|dirs| dirs.data_dir().to_path_buf())
}

/// Moves the data of the former location to the current one, each file only
/// when its new path does not exist yet. A failed move is logged and leaves
/// every old file in place: the navigator then starts fresh at the new path,
/// and nothing is lost.
pub fn migrate_legacy_data() {
    let (Some(old_dir), Some(history), Some(log)) =
        (legacy_data_dir(), history_db(), decision_log())
    else {
        return;
    };
    let moves = [
        (
            migrate_legacy_db(&old_dir.join("navigator").join("sessions.db"), &history),
            "history database",
        ),
        (
            migrate_legacy(&old_dir.join("navigator.jsonl"), &log),
            "decision log",
        ),
    ];
    for (outcome, what) in moves {
        match outcome {
            Ok(true) => tracing::info!("moved the {what} to the navigator data dir"),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                "could not move the {what} to the navigator data dir, starting fresh: {err}"
            ),
        }
    }
}

/// Renames `old` to `new` when `new` does not exist and `old` does.
/// `Ok(true)` when a file was moved. On error `old` is untouched.
pub fn migrate_legacy(old: &Path, new: &Path) -> io::Result<bool> {
    move_group(&[(old.to_path_buf(), new.to_path_buf())])
}

/// [`migrate_legacy`] for a SQLite database: the `-wal` and `-shm` siblings
/// move with it when present. All or nothing: if any rename fails, the ones
/// already done are undone, so the old database stays whole.
pub fn migrate_legacy_db(old: &Path, new: &Path) -> io::Result<bool> {
    let mut pairs = Vec::new();
    for suffix in ["-wal", "-shm"] {
        pairs.push((with_suffix(old, suffix), with_suffix(new, suffix)));
    }
    // The main file goes last: the new path only appears once the siblings
    // are in place.
    pairs.push((old.to_path_buf(), new.to_path_buf()));
    move_group(&pairs)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Moves the present sources of `pairs`, the last pair being the main file.
/// Does nothing unless the main file exists and its destination does not.
fn move_group(pairs: &[(PathBuf, PathBuf)]) -> io::Result<bool> {
    let Some((main_old, main_new)) = pairs.last() else {
        return Ok(false);
    };
    if main_new.exists() || !main_old.exists() {
        return Ok(false);
    }
    if let Some(parent) = main_new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for pair in pairs {
        if !pair.0.exists() {
            continue;
        }
        if let Err(err) = std::fs::rename(&pair.0, &pair.1) {
            for (old, new) in done.into_iter().rev() {
                let _ = std::fs::rename(new, old);
            }
            return Err(err);
        }
        done.push(pair);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a data dir name and a file below it
    // When the paths resolve
    // Then the database and the log sit side by side in the navigator dir
    #[test]
    fn data_paths_share_the_navigator_dir() {
        let (Some(dir), Some(db), Some(log)) = (data_dir(), history_db(), decision_log()) else {
            return; // no home directory in this environment
        };
        assert!(dir.ends_with("navigator"));
        assert_eq!(db, dir.join("navigator.db"));
        assert_eq!(log, dir.join("navigator.jsonl"));
    }

    // Given an old file and no new one
    // When it is migrated
    // Then it moves, creating the new parent dir
    #[test]
    fn migrate_moves_the_old_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("old.jsonl");
        let new = tmp.path().join("deep").join("new.jsonl");
        std::fs::write(&old, "data").expect("write");

        assert!(migrate_legacy(&old, &new).expect("migrate"));
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&new).expect("read"), "data");
    }

    // Given a new file already present
    // When the old one is migrated
    // Then neither is touched
    #[test]
    fn migrate_never_overwrites_the_new_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::write(&old, "old").expect("write");
        std::fs::write(&new, "new").expect("write");

        assert!(!migrate_legacy(&old, &new).expect("migrate"));
        assert_eq!(std::fs::read_to_string(&old).expect("read"), "old");
        assert_eq!(std::fs::read_to_string(&new).expect("read"), "new");
    }

    // Given no old file
    // When migrating
    // Then nothing happens and nothing is created
    #[test]
    fn migrate_without_an_old_file_does_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let new = tmp.path().join("sub").join("new");
        assert!(!migrate_legacy(&tmp.path().join("old"), &new).expect("migrate"));
        assert!(!new.exists());
    }

    // Given a database with its wal and shm siblings
    // When migrated
    // Then all three move together
    #[test]
    fn migrate_db_takes_the_siblings_along() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("sessions.db");
        let new = tmp.path().join("nav").join("navigator.db");
        for (suffix, body) in [("", "db"), ("-wal", "wal"), ("-shm", "shm")] {
            std::fs::write(with_suffix(&old, suffix), body).expect("write");
        }

        assert!(migrate_legacy_db(&old, &new).expect("migrate"));
        for (suffix, body) in [("", "db"), ("-wal", "wal"), ("-shm", "shm")] {
            assert!(!with_suffix(&old, suffix).exists());
            let moved = std::fs::read_to_string(with_suffix(&new, suffix)).expect("read");
            assert_eq!(moved, body);
        }
    }

    // Given a database without siblings
    // When migrated
    // Then only the main file moves
    #[test]
    fn migrate_db_without_siblings_moves_the_main_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("sessions.db");
        let new = tmp.path().join("navigator.db");
        std::fs::write(&old, "db").expect("write");

        assert!(migrate_legacy_db(&old, &new).expect("migrate"));
        assert!(new.exists() && !old.exists());
        assert!(!with_suffix(&new, "-wal").exists());
    }

    // Given a database whose -shm cannot be moved (its destination is a
    //       non-empty directory)
    // When migrated
    // Then it fails and every old file is back in place: no data loss
    #[test]
    fn a_failed_rename_leaves_every_old_file_in_place() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("sessions.db");
        let new = tmp.path().join("navigator.db");
        for (suffix, body) in [("", "db"), ("-wal", "wal"), ("-shm", "shm")] {
            std::fs::write(with_suffix(&old, suffix), body).expect("write");
        }
        let blocker = with_suffix(&new, "-shm");
        std::fs::create_dir(&blocker).expect("blocker dir");
        std::fs::write(blocker.join("x"), "x").expect("blocker content");

        assert!(migrate_legacy_db(&old, &new).is_err());
        for (suffix, body) in [("", "db"), ("-wal", "wal"), ("-shm", "shm")] {
            let kept = std::fs::read_to_string(with_suffix(&old, suffix)).expect("old kept");
            assert_eq!(kept, body);
        }
        assert!(!new.exists(), "the new database never appeared");
        assert!(!with_suffix(&new, "-wal").exists());
    }
}
