//! Local instructions: the `AGENTS.md` / `CLAUDE.md` / `rules.md` files a
//! session reads once, when it is created or resumed, and sends to the model
//! as one source-labelled system message.
//!
//! Discovery, in this order (later text is closer to the work):
//!
//! 1. the global file: `<home>/.agents/AGENTS.md`, else
//!    `<home>/.claude/CLAUDE.md`;
//! 2. the optional global supplement `<home>/.agents/rules.md`;
//! 3. for each directory from the worktree root down to the workspace:
//!    `AGENTS.md`, else `CLAUDE.md`, then the optional `rules.md`.
//!
//! The worktree root is the nearest ancestor of the (canonical) workspace
//! holding a `.git` directory or a `.git` file (a linked worktree); without
//! Git it is the workspace itself, so nothing above it is ever read. A file
//! reached twice through links is read once.
//!
//! Bounds: [`MAX_FILE_BYTES`] per file and [`MAX_TOTAL_BYTES`] in all, counted
//! on the file bytes kept (the labels and truncation markers are framing).
//! A cut never splits a UTF-8 character and leaves a visible marker. Every
//! source gets a [`RuleSource`] status; a file that is simply absent is
//! normal and reported nowhere.

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most bytes read from one file.
pub const MAX_FILE_BYTES: usize = 16 * 1024;
/// The most file bytes kept across all sources.
pub const MAX_TOTAL_BYTES: usize = 64 * 1024;

/// The framing line that opens the rules message.
const FRAMING: &str = "These local instructions come from the user's and the project's files. \
They apply within the agent contract and your role above and never override either. \
When two files disagree, the one listed later is closer to the working directory and wins.";

/// Where discovery looks beyond the workspace, for the rules and for the
/// skills. Injected so that tests never read the real home directory.
#[derive(Debug, Clone, Default)]
pub struct RulesRoots {
    /// The home directory holding the global files; `None` reads no global
    /// file.
    pub home: Option<PathBuf>,
}

impl RulesRoots {
    /// The current user's home directory, for the binaries.
    pub fn from_user_home() -> Self {
        Self {
            home: directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
        }
    }
}

/// Which bound cut a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// The per-file limit.
    File,
    /// What was left of the total limit.
    Total,
}

/// What became of one discovered source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// Read whole and sent.
    Loaded,
    /// Read, but holds nothing besides whitespace.
    Empty,
    /// Cut at a bound; the message carries a marker.
    Truncated { omitted_bytes: u64, cap: Cap },
    /// `CLAUDE.md` next to an `AGENTS.md`: only one of the two is read.
    Shadowed { by: PathBuf },
    /// The same file, reached through a link, was already read.
    Duplicate { of: PathBuf },
    /// Not read: the total limit was already spent.
    OmittedForTotalCap,
    /// The file exists but could not be read.
    ReadError(String),
}

impl fmt::Display for SourceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loaded => write!(f, "loaded"),
            Self::Empty => write!(f, "empty"),
            Self::Truncated { omitted_bytes, cap } => {
                let cap = match cap {
                    Cap::File => "per-file",
                    Cap::Total => "total",
                };
                write!(
                    f,
                    "truncated ({omitted_bytes} bytes omitted by the {cap} cap)"
                )
            }
            Self::Shadowed { by } => write!(f, "shadowed by {}", by.display()),
            Self::Duplicate { of } => write!(f, "same file as {}", of.display()),
            Self::OmittedForTotalCap => write!(f, "omitted: total cap reached"),
            Self::ReadError(reason) => write!(f, "read error: {reason}"),
        }
    }
}

/// One discovered file and its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSource {
    pub path: PathBuf,
    pub status: SourceStatus,
    /// File bytes kept in the message.
    pub loaded_bytes: usize,
}

impl RuleSource {
    /// Whether the driver should be told: anything but a clean outcome.
    pub fn is_problem(&self) -> bool {
        matches!(
            self.status,
            SourceStatus::Truncated { .. }
                | SourceStatus::OmittedForTotalCap
                | SourceStatus::ReadError(_)
        )
    }
}

/// The result of one discovery: the message to send and the diagnostics.
#[derive(Debug, Clone, Default)]
pub struct Instructions {
    message: Option<String>,
    sources: Vec<RuleSource>,
}

impl Instructions {
    /// Discovers and reads the rules for `workspace`. Blocking file I/O: call
    /// it from a blocking context.
    pub fn load(roots: &RulesRoots, workspace: &Path) -> Self {
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let mut loader = Loader::default();
        if let Some(home) = &roots.home {
            loader.slot(
                &home.join(".agents").join("AGENTS.md"),
                Some(&home.join(".claude").join("CLAUDE.md")),
            );
            loader.slot(&home.join(".agents").join("rules.md"), None);
        }
        for dir in directories_from_root(&workspace) {
            loader.slot(&dir.join("AGENTS.md"), Some(&dir.join("CLAUDE.md")));
            loader.slot(&dir.join("rules.md"), None);
        }
        loader.finish()
    }

    /// The system message text; `None` when no source holds anything.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Every discovered source, in discovery order.
    pub fn sources(&self) -> &[RuleSource] {
        &self.sources
    }

    /// The files whose text is in the message, cut or whole.
    pub fn contributing_files(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| {
                matches!(
                    s.status,
                    SourceStatus::Loaded | SourceStatus::Truncated { .. }
                )
            })
            .count()
    }

    /// File bytes kept in the message.
    pub fn loaded_bytes(&self) -> usize {
        self.sources.iter().map(|s| s.loaded_bytes).sum()
    }
}

/// The directories from the worktree root down to `workspace`, in that order.
pub(crate) fn directories_from_root(workspace: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in workspace.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
    }
    // Without a `.git` the walk reached the filesystem root: keep only the
    // workspace, so that nothing above it is read.
    if !dirs.last().is_some_and(|d| d.join(".git").exists()) {
        dirs = vec![workspace.to_path_buf()];
    }
    dirs.reverse();
    dirs
}

#[derive(Default)]
struct Loader {
    seen: HashSet<PathBuf>,
    used: usize,
    sections: Vec<(PathBuf, String)>,
    sources: Vec<RuleSource>,
}

impl Loader {
    /// One slot: `primary`, else `fallback`. An absent file is silent; when
    /// both exist the fallback is reported as shadowed.
    fn slot(&mut self, primary: &Path, fallback: Option<&Path>) {
        if exists(primary) {
            self.take(primary);
            if let Some(fallback) = fallback.filter(|f| exists(f)) {
                self.push(
                    fallback,
                    SourceStatus::Shadowed {
                        by: primary.to_path_buf(),
                    },
                    0,
                );
            }
        } else if let Some(fallback) = fallback.filter(|f| exists(f)) {
            self.take(fallback);
        }
    }

    fn push(&mut self, path: &Path, status: SourceStatus, loaded_bytes: usize) {
        self.sources.push(RuleSource {
            path: path.to_path_buf(),
            status,
            loaded_bytes,
        });
    }

    fn take(&mut self, path: &Path) {
        let canonical = match std::fs::canonicalize(path) {
            Ok(canonical) => canonical,
            Err(e) => return self.push(path, SourceStatus::ReadError(e.to_string()), 0),
        };
        if !self.seen.insert(canonical.clone()) {
            return self.push(path, SourceStatus::Duplicate { of: canonical }, 0);
        }
        let left = MAX_TOTAL_BYTES - self.used;
        if left == 0 {
            return self.push(path, SourceStatus::OmittedForTotalCap, 0);
        }
        let limit = left.min(MAX_FILE_BYTES);
        let read = match read_bounded(&canonical, limit) {
            Ok(read) => read,
            Err(e) => return self.push(path, SourceStatus::ReadError(e.to_string()), 0),
        };
        self.used += read.kept;
        let mut text = read.text.trim_end().to_string();
        let status = match read.omitted {
            Some(omitted_bytes) => {
                text.push_str(&format!("\n[… truncated: {omitted_bytes} bytes omitted]"));
                let cap = if limit == MAX_FILE_BYTES {
                    Cap::File
                } else {
                    Cap::Total
                };
                SourceStatus::Truncated { omitted_bytes, cap }
            }
            None if text.trim().is_empty() => SourceStatus::Empty,
            None => SourceStatus::Loaded,
        };
        if status != SourceStatus::Empty {
            self.sections.push((path.to_path_buf(), text));
        }
        self.push(path, status, read.kept);
    }

    fn finish(self) -> Instructions {
        let message = (!self.sections.is_empty()).then(|| {
            let mut message = FRAMING.to_string();
            for (path, text) in &self.sections {
                message.push_str(&format!(
                    "\n\nInstructions from {}:\n{text}",
                    path.display()
                ));
            }
            message
        });
        Instructions {
            message,
            sources: self.sources,
        }
    }
}

/// Whether something is at `path`, a dangling link included.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

struct BoundedRead {
    text: String,
    /// Bytes of the file kept.
    kept: usize,
    /// Bytes left out, when the file was cut.
    omitted: Option<u64>,
}

/// Reads at most `limit` bytes of a regular file, cutting on a character
/// boundary. Never reads more than `limit + 1` bytes.
fn read_bounded(path: &Path, limit: usize) -> std::io::Result<BoundedRead> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    let omitted = if bytes.len() > limit {
        let mut cut = limit;
        // Back up over continuation bytes to the start of a character.
        while cut > 0 && bytes[cut] & 0xC0 == 0x80 {
            cut -= 1;
        }
        bytes.truncate(cut);
        Some(meta.len().saturating_sub(cut as u64).max(1))
    } else {
        None
    };
    Ok(BoundedRead {
        kept: bytes.len(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        omitted,
    })
}

#[cfg(test)]
mod tests;
