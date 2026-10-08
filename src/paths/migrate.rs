//! The one-time move of existing user data and configuration to the WithNav
//! dirs, on the first start of a new binary.
//!
//! Sources, in order of priority:
//!
//! - data: `~/.local/share/navigator/*`, then `~/.local/share/rust-agent/*`
//!   (whose database sat in a `navigator/` subdir under another name);
//! - configuration: `~/.config/rust-agent/*`.
//!
//! The rules: a file is only moved when its new path does not exist, so
//! nothing is ever overwritten; a second start finds nothing to do; a SQLite
//! database moves together with its `-wal` and `-shm` files, and only when no
//! other process holds any of them. When that move is refused the database
//! stays where it is and [`crate::paths::history_db`] keeps pointing at it for
//! this process.

use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use directories::ProjectDirs;
use nix::fcntl::{fcntl, FcntlArg};

use super::APP_NAME;

#[cfg(test)]
mod tests;

/// The former application name of the data dir.
const NAVIGATOR_NAME: &str = "navigator";
/// The former application name of the crate, data and config dirs.
const RUST_AGENT_NAME: &str = "rust-agent";

/// The database path kept in use because its move was refused.
static KEPT_HISTORY_DB: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The former database path this process keeps using, if a move was refused.
pub(super) fn kept_history_db() -> Option<PathBuf> {
    KEPT_HISTORY_DB.lock().ok().and_then(|kept| kept.clone())
}

/// The directories involved in a migration.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The WithNav data dir, the target.
    pub data: PathBuf,
    /// The WithNav config dir, the target.
    pub config: PathBuf,
    /// The former `navigator` data dir.
    pub navigator_data: PathBuf,
    /// Compatibility source for the 0.7 series.
    pub navai_data: PathBuf,
    pub navai_config: PathBuf,
    /// The former `rust-agent` data dir.
    pub rust_agent_data: PathBuf,
    /// The former `rust-agent` config dir.
    pub rust_agent_config: PathBuf,
}

impl Layout {
    /// The XDG dirs of the current user, when the home directory resolves.
    fn current() -> Option<Self> {
        let dirs = |name: &str| ProjectDirs::from("", "", name);
        let new = dirs(APP_NAME)?;
        Some(Self {
            data: new.data_dir().to_path_buf(),
            config: new.config_dir().to_path_buf(),
            navigator_data: dirs(NAVIGATOR_NAME)?.data_dir().to_path_buf(),
            navai_data: dirs("navai")?.data_dir().to_path_buf(),
            navai_config: dirs("navai")?.config_dir().to_path_buf(),
            rust_agent_data: dirs(RUST_AGENT_NAME)?.data_dir().to_path_buf(),
            rust_agent_config: dirs(RUST_AGENT_NAME)?.config_dir().to_path_buf(),
        })
    }
}

/// What a migration did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Every file moved, as `(from, to)`.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Moves that were refused or failed; the old files are still in place.
    pub warnings: Vec<String>,
    /// The former database path kept in use, when its move was refused.
    pub kept_history_db: Option<PathBuf>,
}

impl MigrationReport {
    /// Writes the report to the log: one line per file moved, one per
    /// warning. Silent for a start that had nothing to do.
    pub fn log(&self) {
        for (from, to) in &self.moved {
            tracing::info!("moved {} to {}", from.display(), to.display());
        }
        for warning in &self.warnings {
            tracing::warn!("{warning}");
        }
    }
}

/// Moves the data and configuration of the former locations to the WithNav
/// dirs. Call it once, before anything opens a data file or the log.
pub fn migrate_legacy() -> MigrationReport {
    let Some(layout) = Layout::current() else {
        return MigrationReport::default();
    };
    let report = migrate_in(&layout);
    if let (Some(kept), Ok(mut slot)) = (&report.kept_history_db, KEPT_HISTORY_DB.lock()) {
        *slot = Some(kept.clone());
    }
    report
}

/// One file to bring over: its new path and the former candidates in order.
struct Item {
    target: PathBuf,
    sources: Vec<PathBuf>,
    /// A SQLite database: its `-wal` and `-shm` files move with it.
    database: bool,
}

fn items(layout: &Layout) -> Vec<Item> {
    let data = |name: &str, sources: &[PathBuf]| Item {
        target: layout.data.join(name),
        sources: sources.to_vec(),
        database: name.ends_with(".db"),
    };
    let nav = &layout.navigator_data;
    let old = &layout.rust_agent_data;
    let mut items = vec![
        data(
            "withnav.db",
            &[
                layout.navai_data.join("navai.db"),
                nav.join("navigator.db"),
                old.join("navigator/sessions.db"),
            ],
        ),
        data(
            "withnav.jsonl",
            &[
                layout.navai_data.join("navai.jsonl"),
                nav.join("navigator.jsonl"),
                old.join("navigator.jsonl"),
            ],
        ),
        data(
            "tui.log",
            &[
                layout.navai_data.join("tui.log"),
                nav.join("tui.log"),
                old.join("tui.log"),
            ],
        ),
    ];
    for name in ["auth.json", "models.json", "navigator/pipeline.toml"] {
        items.push(Item {
            target: layout.config.join(name),
            sources: vec![layout.rust_agent_config.join(name)],
            database: false,
        });
    }
    for (source, target) in [
        (&layout.navai_config, &layout.config),
        (&layout.navai_data, &layout.data),
    ] {
        collect_legacy_files(source, target, &mut items);
    }
    items
}

fn collect_legacy_files(source: &Path, target: &Path, items: &mut Vec<Item>) {
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        if kind.is_dir() {
            collect_legacy_files(&entry.path(), &target.join(&name), items);
        } else if kind.is_file()
            && !matches!(
                name.to_str(),
                Some("navai.db" | "navai.db-wal" | "navai.db-shm" | "navai.jsonl")
            )
        {
            items.push(Item {
                target: target.join(name),
                sources: vec![entry.path()],
                database: false,
            });
        }
    }
}

/// Moves every present former file of `layout` to its new path.
pub fn migrate_in(layout: &Layout) -> MigrationReport {
    let mut report = MigrationReport::default();
    for item in items(layout) {
        if item.target.exists() {
            continue;
        }
        let Some(source) = item.sources.iter().find(|source| source.exists()) else {
            continue;
        };
        match move_item(source, &item.target, item.database) {
            Ok(moved) => report.moved.extend(moved),
            Err(reason) => {
                report.warnings.push(format!(
                    "did not move {} to {}: {reason}; the old file stays in place",
                    source.display(),
                    item.target.display()
                ));
                if item.database {
                    report.kept_history_db = Some(source.clone());
                }
            }
        }
    }
    report
}

/// Moves `source` to `target`, with its SQLite siblings for a database.
/// Either every file moves or none does.
fn move_item(
    source: &Path,
    target: &Path,
    database: bool,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut pairs = Vec::new();
    if database {
        for suffix in ["-wal", "-shm"] {
            let old = with_suffix(source, suffix);
            if old.exists() {
                pairs.push((old, with_suffix(target, suffix)));
            }
        }
    }
    // The main file goes last: the new database only appears once its
    // siblings are in place.
    pairs.push((source.to_path_buf(), target.to_path_buf()));

    if database {
        if let Some((busy, _)) = pairs.iter().find(|(old, _)| held_elsewhere(old)) {
            return Err(format!("{} is in use by another process", busy.display()));
        }
        // A stray sibling at the new path would be taken for part of the new
        // database.
        if let Some((_, new)) = pairs.iter().find(|(_, new)| new.exists()) {
            return Err(format!("{} already exists", new.display()));
        }
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut placed: Vec<&PathBuf> = Vec::new();
    for (old, new) in &pairs {
        if let Err(err) = place(old, new) {
            for done in placed {
                let _ = fs::remove_file(done);
            }
            return Err(format!("{}: {err}", new.display()));
        }
        placed.push(new);
    }
    for (old, _) in &pairs {
        // A leftover old file is harmless: its new path now exists.
        let _ = fs::remove_file(old);
    }
    Ok(pairs)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Makes `new` a copy of `old` without ever replacing an existing `new`,
/// keeping the file mode. A hard link is atomic and free; when the
/// filesystem cannot link, the content is copied.
fn place(old: &Path, new: &Path) -> io::Result<()> {
    match fs::hard_link(old, new) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
        Err(_) => copy_new(old, new),
    }
}

fn copy_new(old: &Path, new: &Path) -> io::Result<()> {
    let permissions = fs::metadata(old)?.permissions();
    let mut source = fs::File::open(old)?;
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(permissions.mode() & 0o777)
        .open(new)?;
    let copied = io::copy(&mut source, &mut target)
        .and_then(|_| target.sync_all())
        .and_then(|()| fs::set_permissions(new, permissions));
    if copied.is_err() {
        let _ = fs::remove_file(new);
    }
    copied
}

/// True when another process (or another open file description) holds a
/// lock on `path`, or when that cannot be told. SQLite takes POSIX locks on
/// the database and its shared-memory file for as long as a connection is
/// open. Closing the probe's descriptor would drop any lock this process
/// holds on the file, so this must run before the process opens the database.
fn held_elsewhere(path: &Path) -> bool {
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(path) else {
        return true;
    };
    let mut probe = nix::libc::flock {
        l_type: nix::libc::F_WRLCK as i16,
        l_whence: nix::libc::SEEK_SET as i16,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    match fcntl(&file, FcntlArg::F_OFD_GETLK(&mut probe)) {
        Ok(_) => probe.l_type != nix::libc::F_UNLCK as i16,
        Err(_) => true,
    }
}
